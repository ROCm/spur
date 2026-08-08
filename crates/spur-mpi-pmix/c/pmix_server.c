// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#include <pthread.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <pmix.h>
#include <pmix_server.h>
#include <pmix_version.h>

#include "spur_mpi_plugin.h"
#include "modex_exchange.h"

#define SPUR_PMIX_MAX_SESSIONS 64
#define SPUR_PMIX_MAX_LOCAL_PROCS 256

/* fence_local_rank_index: unknown rank vs duplicate wildcard fence */
#define SPUR_FENCE_IDX_UNKNOWN (-1)
#define SPUR_FENCE_IDX_WILDCARD_DUP (-2)

typedef struct {
    bool active;
    bool collecting;
    uint32_t arrivals;
    bool rank_arrived[SPUR_PMIX_MAX_LOCAL_PROCS];
    char *local_data[SPUR_PMIX_MAX_LOCAL_PROCS];
    size_t local_len[SPUR_PMIX_MAX_LOCAL_PROCS];
    pmix_modex_cbfunc_t cbfunc[SPUR_PMIX_MAX_LOCAL_PROCS];
    void *cbdata[SPUR_PMIX_MAX_LOCAL_PROCS];
} spur_local_fence_t;

typedef struct {
    _Atomic int refs;
    size_t len;
    char data[];
} spur_modex_shared_t;

static int pmix_status_ok(pmix_status_t rc) {
    return rc == PMIX_SUCCESS || rc == PMIX_OPERATION_SUCCEEDED;
}

typedef struct {
    int active;
    char namespace_[256];
    char tmpdir[512];
    uint32_t universe_size;
    uint32_t num_local_procs;
    uint32_t local_ranks[256];
    uint32_t num_nodes;
    uint32_t node_index;
    char peer_hosts[SPUR_MPI_MAX_PEER_HOSTS][256];
    uint32_t num_peer_hosts;
    spur_modex_session_t *modex;
    spur_modex_timeouts_t modex_timeouts;
    spur_local_fence_t local_fence;
} spur_pmix_session_t;

static int local_procs_match(const spur_pmix_session_t *session, const spur_mpi_launch_plan_t *plan) {
    if (session->num_local_procs != plan->num_local_procs) {
        return 0;
    }
    for (uint32_t i = 0; i < plan->num_local_procs; i++) {
        if (session->local_ranks[i] != plan->local_procs[i].rank) {
            return 0;
        }
    }
    return 1;
}

static spur_pmix_session_t g_sessions[SPUR_PMIX_MAX_SESSIONS];
static pthread_mutex_t g_session_lock = PTHREAD_MUTEX_INITIALIZER;
static int g_server_initialized = 0;

static pmix_status_t spur_fence_nb(
    const pmix_proc_t procs[],
    size_t nprocs,
    const pmix_info_t info[],
    size_t ninfo,
    char *data,
    size_t ndata,
    pmix_modex_cbfunc_t cbfunc,
    void *cbdata
);

static void spur_modex_release(void *release_cbdata) {
    spur_modex_shared_t *shared = (spur_modex_shared_t *)release_cbdata;
    if (shared == NULL) {
        return;
    }
    if (atomic_fetch_sub(&shared->refs, 1) == 1) {
        free(shared);
    }
}

static spur_modex_shared_t *spur_modex_shared_create(const char *merged, size_t merged_len, int refs) {
    if (refs <= 0) {
        return NULL;
    }
    size_t alloc = merged_len > 0 ? merged_len : 1;
    spur_modex_shared_t *shared = malloc(sizeof(*shared) + alloc);
    if (shared == NULL) {
        return NULL;
    }
    atomic_init(&shared->refs, refs);
    shared->len = merged_len;
    if (merged_len > 0 && merged != NULL) {
        memcpy(shared->data, merged, merged_len);
    }
    return shared;
}

static int local_rank_index(const spur_pmix_session_t *session, uint32_t rank) {
    for (uint32_t i = 0; i < session->num_local_procs; i++) {
        if (session->local_ranks[i] == rank) {
            return (int)i;
        }
    }
    return -1;
}

static int fence_local_rank_index(spur_pmix_session_t *session, uint32_t rank) {
    int idx = local_rank_index(session, rank);
    if (idx >= 0) {
        return idx;
    }
    if (rank != PMIX_RANK_WILDCARD) {
        return -1;
    }
    /* OpenPMIx 5 invokes fence_nb with PMIX_RANK_WILDCARD for the calling client. */
    spur_local_fence_t *fence = &session->local_fence;
    uint32_t num_local = session->num_local_procs;
    if (num_local > SPUR_PMIX_MAX_LOCAL_PROCS) {
        num_local = SPUR_PMIX_MAX_LOCAL_PROCS;
    }
    for (uint32_t i = 0; i < num_local; i++) {
        if (!fence->rank_arrived[i]) {
            return (int)i;
        }
    }
    return SPUR_FENCE_IDX_WILDCARD_DUP;
}

static void clear_local_fence(spur_local_fence_t *fence, uint32_t num_local_procs) {
    if (num_local_procs > SPUR_PMIX_MAX_LOCAL_PROCS) {
        num_local_procs = SPUR_PMIX_MAX_LOCAL_PROCS;
    }
    for (uint32_t i = 0; i < num_local_procs; i++) {
        free(fence->local_data[i]);
        fence->local_data[i] = NULL;
        fence->local_len[i] = 0;
        fence->cbfunc[i] = NULL;
        fence->cbdata[i] = NULL;
        fence->rank_arrived[i] = false;
    }
    fence->active = false;
    fence->collecting = false;
    fence->arrivals = 0;
}

static pmix_status_t modex_rc_to_pmix(int modex_rc) {
    if (modex_rc == SPUR_MODEX_ERR_TIMEOUT) {
        return PMIX_ERR_TIMEOUT;
    }
    if (modex_rc == SPUR_MODEX_ERR_ABORT) {
        return PMIX_ERR_JOB_TERMINATED;
    }
    return PMIX_ERROR;
}

static void finish_local_fence(
    spur_pmix_session_t *session,
    pmix_status_t status,
    char *merged,
    size_t merged_len
) {
    spur_local_fence_t *fence = &session->local_fence;
    uint32_t num_local = session->num_local_procs;
    if (num_local > SPUR_PMIX_MAX_LOCAL_PROCS) {
        num_local = SPUR_PMIX_MAX_LOCAL_PROCS;
    }

    spur_modex_shared_t *shared = NULL;
    if (status == PMIX_SUCCESS) {
        shared = spur_modex_shared_create(merged, merged_len, (int)fence->arrivals);
        free(merged);
        if (shared == NULL) {
            status = PMIX_ERR_NOMEM;
        }
    }

    for (uint32_t i = 0; i < num_local; i++) {
        if (!fence->rank_arrived[i] || fence->cbfunc[i] == NULL) {
            continue;
        }
        if (status == PMIX_SUCCESS && shared != NULL) {
            fence->cbfunc[i](
                PMIX_SUCCESS,
                shared->data,
                shared->len,
                fence->cbdata[i],
                spur_modex_release,
                shared
            );
        } else {
            fence->cbfunc[i](status, NULL, 0, fence->cbdata[i], NULL, NULL);
        }
    }

    if (status != PMIX_SUCCESS) {
        free(merged);
    }

    clear_local_fence(fence, num_local);
}

static int concat_local_blobs(
    const spur_pmix_session_t *session,
    const spur_local_fence_t *fence,
    char **out_data,
    size_t *out_len
) {
    size_t total = 0;
    uint32_t num_local = session->num_local_procs;
    if (num_local > SPUR_PMIX_MAX_LOCAL_PROCS) {
        return -1;
    }
    for (uint32_t i = 0; i < num_local; i++) {
        total += fence->local_len[i];
    }
    if (total > SPUR_MODEX_MAX_BLOB) {
        return -1;
    }

    char *merged = malloc(total > 0 ? total : 1);
    if (merged == NULL) {
        return -1;
    }
    size_t offset = 0;
    for (uint32_t i = 0; i < num_local; i++) {
        if (fence->local_len[i] > 0 && fence->local_data[i] != NULL) {
            memcpy(merged + offset, fence->local_data[i], fence->local_len[i]);
            offset += fence->local_len[i];
        }
    }
    *out_data = merged;
    *out_len = offset;
    return 0;
}

static spur_modex_timeouts_t modex_timeouts_from_plan(const spur_mpi_launch_plan_t *plan) {
    return (spur_modex_timeouts_t){
        .connect_sec = plan->modex_connect_timeout_sec,
        .fence_sec = plan->modex_fence_timeout_sec,
        .verify_sec = plan->modex_verify_timeout_sec,
    };
}

static spur_pmix_session_t *find_session(const char *namespace_);

static void spur_mpi_debug(const char *fmt, ...) {
    if (getenv("SPUR_MPI_DEBUG") == NULL) {
        return;
    }
    va_list args;
    va_start(args, fmt);
    vfprintf(stderr, fmt, args);
    va_end(args);
}

static pmix_status_t spur_client_connected(
    const pmix_proc_t *proc,
    void *server_object,
    pmix_op_cbfunc_t cbfunc,
    void *cbdata
) {
    (void)proc;
    (void)server_object;
    if (cbfunc != NULL) {
#if PMIX_VERSION_MAJOR >= 6
        cbfunc(PMIX_SUCCESS, NULL, 0, cbdata, NULL, NULL);
#else
        cbfunc(PMIX_SUCCESS, cbdata);
#endif
    }
    return PMIX_SUCCESS;
}

static pmix_status_t spur_client_finalized(
    const pmix_proc_t *proc,
    void *server_object,
    pmix_op_cbfunc_t cbfunc,
    void *cbdata
) {
    (void)proc;
    (void)server_object;
    if (cbfunc != NULL) {
#if PMIX_VERSION_MAJOR >= 6
        cbfunc(PMIX_SUCCESS, NULL, 0, cbdata, NULL, NULL);
#else
        cbfunc(PMIX_SUCCESS, cbdata);
#endif
    }
    return PMIX_SUCCESS;
}

/*
 * Always register fence_nb at PMIx_server_init. Single-node jobs never call it;
 * multi-node jobs use it for TCP modex exchange. Toggling fence_nb between jobs
 * via PMIx_server_finalize/reinit breaks PMIx_server_setup_fork on OpenPMIx 5.x.
 */
static pmix_server_module_t spur_module = {
    .client_connected = spur_client_connected,
    .client_finalized = spur_client_finalized,
    .fence_nb = spur_fence_nb,
};

static int ensure_server_init(const spur_mpi_launch_plan_t *plan, char *errbuf, size_t errlen) {
    (void)plan;
    if (g_server_initialized) {
        return 0;
    }

    spur_module.fence_nb = spur_fence_nb;
    pmix_status_t rc = PMIx_server_init(&spur_module, NULL, 0);
    if (!pmix_status_ok(rc)) {
        if (errbuf != NULL && errlen > 0) {
            snprintf(errbuf, errlen, "PMIx_server_init failed: %s", PMIx_Error_string(rc));
        }
        return -1;
    }
    g_server_initialized = 1;
    spur_mpi_debug("spur_mpi_pmix: PMIx_server_init ok\n");
    return 0;
}

static spur_pmix_session_t *find_session_by_proc(const pmix_proc_t *proc) {
    if (proc == NULL) {
        return NULL;
    }
    return find_session(proc->nspace);
}

static pmix_status_t spur_fence_nb(
    const pmix_proc_t procs[],
    size_t nprocs,
    const pmix_info_t info[],
    size_t ninfo,
    char *data,
    size_t ndata,
    pmix_modex_cbfunc_t cbfunc,
    void *cbdata
) {
    (void)info;
    (void)ninfo;
    if (cbfunc == NULL || procs == NULL) {
        return PMIX_ERR_BAD_PARAM;
    }
    spur_mpi_debug(
        "spur_mpi_pmix: fence_nb enter nprocs=%zu ndata=%zu rank=%u nspace=%s\n",
        nprocs,
        ndata,
        procs[0].rank,
        procs[0].nspace
    );

    pthread_mutex_lock(&g_session_lock);
    spur_pmix_session_t *session = find_session_by_proc(&procs[0]);
    if (session == NULL) {
        pthread_mutex_unlock(&g_session_lock);
        cbfunc(PMIX_ERR_NOT_FOUND, NULL, 0, cbdata, NULL, NULL);
        return PMIX_SUCCESS;
    }
    if (session->modex == NULL) {
        pthread_mutex_unlock(&g_session_lock);
        cbfunc(PMIX_ERR_NOT_SUPPORTED, NULL, 0, cbdata, NULL, NULL);
        return PMIX_SUCCESS;
    }

    int rank_idx = fence_local_rank_index(session, procs[0].rank);
    if (rank_idx == SPUR_FENCE_IDX_WILDCARD_DUP) {
        pthread_mutex_unlock(&g_session_lock);
        cbfunc(PMIX_ERR_BAD_PARAM, NULL, 0, cbdata, NULL, NULL);
        return PMIX_SUCCESS;
    }
    if (rank_idx < 0) {
        spur_mpi_debug(
            "spur_mpi_pmix: fence_nb unknown rank=%u local_procs=%u\n",
            procs[0].rank,
            session->num_local_procs
        );
        pthread_mutex_unlock(&g_session_lock);
        cbfunc(PMIX_ERR_NOT_FOUND, NULL, 0, cbdata, NULL, NULL);
        return PMIX_SUCCESS;
    }

    spur_local_fence_t *fence = &session->local_fence;
    if (fence->active && fence->collecting) {
        pthread_mutex_unlock(&g_session_lock);
        cbfunc(PMIX_ERR_BAD_PARAM, NULL, 0, cbdata, NULL, NULL);
        return PMIX_SUCCESS;
    }
    if (fence->active && fence->rank_arrived[rank_idx]) {
        pthread_mutex_unlock(&g_session_lock);
        cbfunc(PMIX_ERR_BAD_PARAM, NULL, 0, cbdata, NULL, NULL);
        return PMIX_SUCCESS;
    }
    if (!fence->active) {
        fence->active = true;
        fence->arrivals = 0;
    }

    char *local_copy = NULL;
    if (ndata > 0 && data != NULL) {
        local_copy = malloc(ndata);
        if (local_copy == NULL) {
            fence->cbfunc[rank_idx] = cbfunc;
            fence->cbdata[rank_idx] = cbdata;
            fence->rank_arrived[rank_idx] = true;
            fence->arrivals++;
            finish_local_fence(session, PMIX_ERR_NOMEM, NULL, 0);
            pthread_mutex_unlock(&g_session_lock);
            return PMIX_SUCCESS;
        }
        memcpy(local_copy, data, ndata);
    }

    fence->local_data[rank_idx] = local_copy;
    fence->local_len[rank_idx] = ndata;
    fence->cbfunc[rank_idx] = cbfunc;
    fence->cbdata[rank_idx] = cbdata;
    fence->rank_arrived[rank_idx] = true;
    fence->arrivals++;

    /* OpenPMIx 5 (Crusoe OMPI 4.1.7): one PMIX_RANK_WILDCARD fence per node when
     * num_local_procs > 1; ndata already holds all local modex. Explicit-rank
     * callers still wait for every local arrival. */
    bool wildcard_fence = (procs[0].rank == PMIX_RANK_WILDCARD);
    if (!(wildcard_fence && session->num_local_procs > 1)
        && fence->arrivals < session->num_local_procs)
    {
        pthread_mutex_unlock(&g_session_lock);
        return PMIX_SUCCESS;
    }

    char *local_merged = NULL;
    size_t local_merged_len = 0;
    if (concat_local_blobs(session, fence, &local_merged, &local_merged_len) != 0) {
        finish_local_fence(session, PMIX_ERR_NOMEM, NULL, 0);
        pthread_mutex_unlock(&g_session_lock);
        return PMIX_SUCCESS;
    }

    /* Drop g_session_lock before the blocking TCP collect; retain modex until collect finishes. */
    spur_modex_session_t *modex = session->modex;
    char *merged = NULL;
    size_t merged_len = 0;
    fence->collecting = true;
    spur_modex_session_retain(modex);
    pthread_mutex_unlock(&g_session_lock);
    int modex_rc = spur_modex_fence_collect(modex, local_merged, local_merged_len, &merged, &merged_len);
    free(local_merged);
    spur_modex_session_release(modex);

    pthread_mutex_lock(&g_session_lock);
    session = find_session_by_proc(&procs[0]);
    if (session == NULL || !session->local_fence.active) {
        pthread_mutex_unlock(&g_session_lock);
        free(merged);
        return PMIX_SUCCESS;
    }
    if (modex_rc != SPUR_MODEX_OK) {
        pmix_status_t perr = modex_rc_to_pmix(modex_rc);
        spur_mpi_debug(
            "spur_mpi_pmix: modex fence failed: %s\n",
            spur_modex_strerror(modex_rc)
        );
        finish_local_fence(session, perr, NULL, 0);
        pthread_mutex_unlock(&g_session_lock);
        return PMIX_SUCCESS;
    }

    finish_local_fence(session, PMIX_SUCCESS, merged, merged_len);
    spur_mpi_debug(
        "spur_mpi_pmix: modex fence ok node=%u merged_len=%zu local_procs=%u\n",
        session->node_index,
        merged_len,
        session->num_local_procs
    );
    pthread_mutex_unlock(&g_session_lock);
    return PMIX_SUCCESS;
}

static spur_pmix_session_t *find_session(const char *namespace_) {
    for (size_t i = 0; i < SPUR_PMIX_MAX_SESSIONS; i++) {
        if (g_sessions[i].active && strncmp(g_sessions[i].namespace_, namespace_, 255) == 0) {
            return &g_sessions[i];
        }
    }
    return NULL;
}

static spur_pmix_session_t *alloc_session(const char *namespace_) {
    for (size_t i = 0; i < SPUR_PMIX_MAX_SESSIONS; i++) {
        if (!g_sessions[i].active) {
            memset(&g_sessions[i], 0, sizeof(g_sessions[i]));
            g_sessions[i].active = 1;
            strncpy(g_sessions[i].namespace_, namespace_, sizeof(g_sessions[i].namespace_) - 1);
            g_sessions[i].namespace_[sizeof(g_sessions[i].namespace_) - 1] = '\0';
            return &g_sessions[i];
        }
    }
    return NULL;
}

#if PMIX_VERSION_MAJOR < 6
static void spur_info_set_uint32(pmix_info_t *info, const char *key, uint32_t value) {
    PMIX_INFO_CONSTRUCT(info);
    strncpy(info->key, key, PMIX_MAX_KEYLEN);
    info->value.type = PMIX_UINT32;
    info->value.data.uint32 = value;
}

static int spur_info_set_string(pmix_info_t *info, const char *key, const char *value) {
    PMIX_INFO_CONSTRUCT(info);
    strncpy(info->key, key, PMIX_MAX_KEYLEN);
    info->value.type = PMIX_STRING;
    info->value.data.string = strdup(value);
    return info->value.data.string != NULL ? 0 : -1;
}

static void spur_info_release(pmix_info_t *info, size_t count) {
    for (size_t i = 0; i < count; i++) {
        if (info[i].value.type == PMIX_STRING && info[i].value.data.string != NULL) {
            free(info[i].value.data.string);
            info[i].value.data.string = NULL;
        }
    }
}

static pmix_status_t spur_load_nspace_info(
    const spur_mpi_launch_plan_t *plan,
    pmix_info_t *info,
    size_t info_cap,
    size_t *ninfo_out,
    char **node_map_out,
    char **proc_map_out,
    char **local_peers_out
) {
    if (plan == NULL || info == NULL || ninfo_out == NULL) {
        return PMIX_ERR_BAD_PARAM;
    }
    *ninfo_out = 0;
    if (node_map_out != NULL) {
        *node_map_out = NULL;
    }
    if (proc_map_out != NULL) {
        *proc_map_out = NULL;
    }
    if (local_peers_out != NULL) {
        *local_peers_out = NULL;
    }

    if (plan->num_nodes == 0 || plan->num_peer_hosts != plan->num_nodes) {
        return PMIX_ERR_BAD_PARAM;
    }
    if (plan->num_local_procs == 0 || plan->universe_size == 0) {
        return PMIX_ERR_BAD_PARAM;
    }

    uint32_t tasks_per_node = plan->universe_size / plan->num_nodes;
    if (tasks_per_node == 0 || tasks_per_node * plan->num_nodes != plan->universe_size) {
        return PMIX_ERR_NOT_SUPPORTED;
    }

    size_t hostlist_cap = 1;
    for (uint32_t i = 0; i < plan->num_peer_hosts; i++) {
        hostlist_cap += strlen(plan->peer_hosts[i]) + 1;
    }
    char *hostlist = calloc(hostlist_cap, 1);
    if (hostlist == NULL) {
        return PMIX_ERR_NOMEM;
    }
    for (uint32_t i = 0; i < plan->num_peer_hosts; i++) {
        if (i > 0) {
            strcat(hostlist, ",");
        }
        strcat(hostlist, plan->peer_hosts[i]);
    }

    char *node_map = NULL;
    pmix_status_t rc = PMIx_generate_regex(hostlist, &node_map);
    free(hostlist);
    if (!pmix_status_ok(rc) || node_map == NULL) {
        free(node_map);
        return rc != PMIX_SUCCESS ? rc : PMIX_ERR_NOMEM;
    }

    size_t proc_map_cap = (size_t)plan->universe_size * 16 + plan->num_nodes + 1;
    char *proc_map_plain = calloc(proc_map_cap, 1);
    if (proc_map_plain == NULL) {
        free(node_map);
        return PMIX_ERR_NOMEM;
    }
    char *pos = proc_map_plain;
    size_t remaining = proc_map_cap;
    for (uint32_t node = 0; node < plan->num_nodes; node++) {
        if (node > 0) {
            if (remaining < 2) {
                free(node_map);
                free(proc_map_plain);
                return PMIX_ERR_NOMEM;
            }
            *pos++ = ';';
            remaining--;
        }
        for (uint32_t t = 0; t < tasks_per_node; t++) {
            uint32_t rank = node * tasks_per_node + t;
            int written = snprintf(pos, remaining, "%s%u", (t > 0 ? "," : ""), rank);
            if (written < 0 || (size_t)written >= remaining) {
                free(node_map);
                free(proc_map_plain);
                return PMIX_ERR_NOMEM;
            }
            pos += written;
            remaining -= (size_t)written;
        }
    }

    char *proc_map = NULL;
    rc = PMIx_generate_ppn(proc_map_plain, &proc_map);
    free(proc_map_plain);
    if (!pmix_status_ok(rc) || proc_map == NULL) {
        free(node_map);
        free(proc_map);
        return rc != PMIX_SUCCESS ? rc : PMIX_ERR_NOMEM;
    }

    size_t local_peers_cap = (size_t)plan->num_local_procs * 12 + 1;
    char *local_peers = calloc(local_peers_cap, 1);
    if (local_peers == NULL) {
        free(node_map);
        free(proc_map);
        return PMIX_ERR_NOMEM;
    }
    uint32_t local_ldr = plan->local_procs[0].rank;
    for (uint32_t i = 0; i < plan->num_local_procs; i++) {
        if (i > 0) {
            strcat(local_peers, ",");
        }
        char rank_buf[16];
        snprintf(rank_buf, sizeof(rank_buf), "%u", plan->local_procs[i].rank);
        strcat(local_peers, rank_buf);
        if (plan->local_procs[i].rank < local_ldr) {
            local_ldr = plan->local_procs[i].rank;
        }
    }

    size_t idx = 0;
    uint32_t univ = plan->universe_size;
    uint32_t local_size = plan->num_local_procs;
    uint32_t node_size = tasks_per_node;
    spur_info_set_uint32(&info[idx], PMIX_UNIV_SIZE, univ);
    idx++;
    spur_info_set_uint32(&info[idx], PMIX_JOB_SIZE, univ);
    idx++;
    spur_info_set_uint32(&info[idx], PMIX_LOCAL_SIZE, local_size);
    idx++;
    spur_info_set_uint32(&info[idx], PMIX_NODE_SIZE, node_size);
    idx++;
    spur_info_set_uint32(&info[idx], PMIX_MAX_PROCS, univ);
    idx++;
    spur_info_set_uint32(&info[idx], PMIX_APP_SIZE, univ);
    idx++;
    if (spur_info_set_string(&info[idx], PMIX_NODE_MAP, node_map) != 0) {
        spur_info_release(info, idx);
        free(node_map);
        free(proc_map);
        free(local_peers);
        return PMIX_ERR_NOMEM;
    }
    idx++;
    if (spur_info_set_string(&info[idx], PMIX_PROC_MAP, proc_map) != 0) {
        spur_info_release(info, idx);
        free(node_map);
        free(proc_map);
        free(local_peers);
        return PMIX_ERR_NOMEM;
    }
    idx++;
    if (spur_info_set_string(&info[idx], PMIX_LOCAL_PEERS, local_peers) != 0) {
        spur_info_release(info, idx);
        free(node_map);
        free(proc_map);
        free(local_peers);
        return PMIX_ERR_NOMEM;
    }
    idx++;
    spur_info_set_uint32(&info[idx], PMIX_LOCALLDR, local_ldr);
    idx++;
    if (spur_info_set_string(&info[idx], PMIX_TMPDIR, plan->tmpdir) != 0) {
        spur_info_release(info, idx);
        free(node_map);
        free(proc_map);
        free(local_peers);
        return PMIX_ERR_NOMEM;
    }
    idx++;

    if (idx > info_cap) {
        spur_info_release(info, idx);
        free(node_map);
        free(proc_map);
        free(local_peers);
        return PMIX_ERR_NOMEM;
    }

    *ninfo_out = idx;
    if (node_map_out != NULL) {
        *node_map_out = node_map;
    } else {
        free(node_map);
    }
    if (proc_map_out != NULL) {
        *proc_map_out = proc_map;
    } else {
        free(proc_map);
    }
    if (local_peers_out != NULL) {
        *local_peers_out = local_peers;
    } else {
        free(local_peers);
    }
    return PMIX_SUCCESS;
}
#endif

static pmix_status_t spur_register_nspace(const spur_mpi_launch_plan_t *plan) {
#if PMIX_VERSION_MAJOR >= 6
    pmix_proc_t *procs = calloc(plan->num_local_procs, sizeof(pmix_proc_t));
    if (procs == NULL) {
        return PMIX_ERR_NOMEM;
    }
    for (uint32_t i = 0; i < plan->num_local_procs; i++) {
        PMIX_LOAD_PROCID(&procs[i], plan->namespace_, plan->local_procs[i].rank);
    }
    pmix_status_t rc = PMIx_server_register_nspace(
        plan->namespace_,
        plan->universe_size,
        procs,
        NULL,
        0
    );
    free(procs);
    return rc;
#else
    pmix_info_t info[12];
    size_t ninfo = 0;
    char *node_map = NULL;
    char *proc_map = NULL;
    char *local_peers = NULL;
    pmix_status_t rc = spur_load_nspace_info(
        plan,
        info,
        sizeof(info) / sizeof(info[0]),
        &ninfo,
        &node_map,
        &proc_map,
        &local_peers
    );
    if (!pmix_status_ok(rc)) {
        return rc;
    }
    rc = PMIx_server_register_nspace(
        plan->namespace_,
        (int)plan->num_local_procs,
        info,
        ninfo,
        NULL,
        NULL
    );
    spur_info_release(info, ninfo);
    free(node_map);
    free(proc_map);
    free(local_peers);
    return rc;
#endif
}

static void spur_deregister_nspace(const char *namespace_) {
#if PMIX_VERSION_MAJOR >= 6
    (void)PMIx_server_deregister_nspace(namespace_);
#else
    PMIx_server_deregister_nspace(namespace_, NULL, NULL);
#endif
}

static uid_t spur_job_uid(const spur_mpi_launch_plan_t *plan) {
    return plan->job_uid != SPUR_MPI_JOB_CRED_UNSET ? (uid_t)plan->job_uid : getuid();
}

static gid_t spur_job_gid(const spur_mpi_launch_plan_t *plan) {
    return plan->job_gid != SPUR_MPI_JOB_CRED_UNSET ? (gid_t)plan->job_gid : getgid();
}

static void spur_deregister_client(const pmix_proc_t *proc) {
    PMIx_server_deregister_client(proc, NULL, NULL);
}

static pmix_status_t spur_register_clients(const spur_mpi_launch_plan_t *plan) {
    uid_t uid = spur_job_uid(plan);
    gid_t gid = spur_job_gid(plan);
    for (uint32_t i = 0; i < plan->num_local_procs; i++) {
        pmix_proc_t proc;
        PMIX_LOAD_PROCID(&proc, plan->namespace_, plan->local_procs[i].rank);
        pmix_status_t rc = PMIx_server_register_client(&proc, uid, gid, NULL, NULL, NULL);
        if (!pmix_status_ok(rc)) {
            for (uint32_t j = 0; j < i; j++) {
                pmix_proc_t prior;
                PMIX_LOAD_PROCID(&prior, plan->namespace_, plan->local_procs[j].rank);
                spur_deregister_client(&prior);
            }
            return rc;
        }
    }
    return PMIX_SUCCESS;
}

int spur_mpi_pmix_version(void) {
    return SPUR_MPI_PLUGIN_API_VERSION;
}

int spur_mpi_pmix_runtime_version(char *buf, size_t buflen) {
    if (buf == NULL || buflen == 0) {
        return -1;
    }
    const char *version = PMIx_Get_version();
    if (version == NULL) {
        buf[0] = '\0';
        return -1;
    }
    snprintf(buf, buflen, "%s", version);
    return 0;
}

int spur_mpi_pmix_server_start(const spur_mpi_launch_plan_t *plan, char *errbuf, size_t errlen) {
    if (plan == NULL) {
        if (errbuf != NULL && errlen > 0) {
            snprintf(errbuf, errlen, "missing PMIx launch plan");
        }
        return -1;
    }

    pthread_mutex_lock(&g_session_lock);
    if (ensure_server_init(plan, errbuf, errlen) != 0) {
        pthread_mutex_unlock(&g_session_lock);
        return -1;
    }

    spur_pmix_session_t *existing = find_session(plan->namespace_);
    if (existing != NULL) {
        if (existing->universe_size != plan->universe_size) {
            if (errbuf != NULL && errlen > 0) {
                snprintf(
                    errbuf,
                    errlen,
                    "namespace %s already registered with universe_size %u (requested %u)",
                    plan->namespace_,
                    existing->universe_size,
                    plan->universe_size
                );
            }
            pthread_mutex_unlock(&g_session_lock);
            return -1;
        }
        if (!local_procs_match(existing, plan)) {
            if (errbuf != NULL && errlen > 0) {
                snprintf(
                    errbuf,
                    errlen,
                    "namespace %s already registered with %u local procs (requested %u)",
                    plan->namespace_,
                    existing->num_local_procs,
                    plan->num_local_procs
                );
            }
            pthread_mutex_unlock(&g_session_lock);
            return -1;
        }
        spur_mpi_debug(
            "spur_mpi_pmix: namespace %s already registered\n",
            plan->namespace_
        );
        pthread_mutex_unlock(&g_session_lock);
        return 0;
    }

    pmix_status_t rc = spur_register_nspace(plan);
    if (!pmix_status_ok(rc)) {
        if (errbuf != NULL && errlen > 0) {
            snprintf(
                errbuf,
                errlen,
                "PMIx_server_register_nspace failed: %s",
                PMIx_Error_string(rc)
            );
        }
        pthread_mutex_unlock(&g_session_lock);
        return -1;
    }

    rc = spur_register_clients(plan);
    if (!pmix_status_ok(rc)) {
        spur_deregister_nspace(plan->namespace_);
        if (errbuf != NULL && errlen > 0) {
            snprintf(
                errbuf,
                errlen,
                "PMIx_server_register_client failed: %s",
                PMIx_Error_string(rc)
            );
        }
        pthread_mutex_unlock(&g_session_lock);
        return -1;
    }

    spur_pmix_session_t *session = alloc_session(plan->namespace_);
    if (session == NULL) {
        spur_deregister_nspace(plan->namespace_);
        if (errbuf != NULL && errlen > 0) {
            snprintf(errbuf, errlen, "PMIx session table full");
        }
        pthread_mutex_unlock(&g_session_lock);
        return -1;
    }

    session->universe_size = plan->universe_size;
    session->num_local_procs = plan->num_local_procs;
    session->num_nodes = plan->num_nodes > 0 ? plan->num_nodes : 1;
    session->node_index = plan->node_index;
    session->num_peer_hosts = plan->num_peer_hosts;
    session->modex = NULL;
    for (uint32_t i = 0; i < plan->num_local_procs && i < 256; i++) {
        session->local_ranks[i] = plan->local_procs[i].rank;
    }
    for (uint32_t i = 0; i < plan->num_peer_hosts && i < SPUR_MPI_MAX_PEER_HOSTS; i++) {
        strncpy(
            session->peer_hosts[i],
            plan->peer_hosts[i],
            sizeof(session->peer_hosts[i]) - 1
        );
        session->peer_hosts[i][sizeof(session->peer_hosts[i]) - 1] = '\0';
    }
    strncpy(session->tmpdir, plan->tmpdir, sizeof(session->tmpdir) - 1);
    session->tmpdir[sizeof(session->tmpdir) - 1] = '\0';

    if (session->num_nodes > 1) {
        session->modex_timeouts = modex_timeouts_from_plan(plan);
        session->modex = spur_modex_session_create(
            plan->job_id,
            session->num_nodes,
            session->node_index,
            session->peer_hosts,
            session->num_peer_hosts,
            &session->modex_timeouts
        );
        if (session->modex == NULL || spur_modex_session_start(session->modex) != SPUR_MODEX_OK) {
            if (session->modex != NULL) {
                spur_modex_session_destroy(session->modex);
                session->modex = NULL;
            }
            spur_deregister_nspace(plan->namespace_);
            session->active = 0;
            session->namespace_[0] = '\0';
            if (errbuf != NULL && errlen > 0) {
                snprintf(errbuf, errlen, "PMIx modex listener failed to start");
            }
            pthread_mutex_unlock(&g_session_lock);
            return -1;
        }
    }

    spur_mpi_debug(
        "spur_mpi_pmix: registered namespace %s size=%u local_procs=%u nodes=%u\n",
        plan->namespace_,
        plan->universe_size,
        plan->num_local_procs,
        session->num_nodes
    );
    pthread_mutex_unlock(&g_session_lock);
    return 0;
}

int spur_mpi_pmix_server_stop(const char *namespace_, char *errbuf, size_t errlen) {
    if (namespace_ == NULL) {
        return 0;
    }

    pthread_mutex_lock(&g_session_lock);
    spur_pmix_session_t *session = find_session(namespace_);
    if (session != NULL) {
        if (session->local_fence.active) {
            finish_local_fence(session, PMIX_ERR_JOB_TERMINATED, NULL, 0);
        } else {
            clear_local_fence(&session->local_fence, session->num_local_procs);
        }
        if (session->modex != NULL) {
            spur_modex_session_t *modex = session->modex;
            session->modex = NULL;
            spur_modex_session_retain(modex);
            spur_modex_session_abort(modex);
            spur_modex_session_release(modex);
        }
#if PMIX_VERSION_MAJOR >= 6
        pmix_status_t rc = PMIx_server_deregister_nspace(namespace_);
        if (!pmix_status_ok(rc)) {
            if (errbuf != NULL && errlen > 0) {
                snprintf(
                    errbuf,
                    errlen,
                    "PMIx_server_deregister_nspace failed: %s",
                    PMIx_Error_string(rc)
                );
            }
            pthread_mutex_unlock(&g_session_lock);
            return -1;
        }
#else
        spur_deregister_nspace(namespace_);
#endif
        session->active = 0;
        session->namespace_[0] = '\0';
        spur_mpi_debug("spur_mpi_pmix: deregistered namespace %s\n", namespace_);
    }
    pthread_mutex_unlock(&g_session_lock);

    if (errbuf != NULL && errlen > 0) {
        errbuf[0] = '\0';
    }
    return 0;
}

int spur_mpi_pmix_verify_peers(const spur_mpi_launch_plan_t *plan, char *errbuf, size_t errlen) {
    if (plan == NULL) {
        if (errbuf != NULL && errlen > 0) {
            snprintf(errbuf, errlen, "missing PMIx launch plan");
        }
        return -1;
    }
    if (plan->num_nodes <= 1) {
        return 0;
    }

    pthread_mutex_lock(&g_session_lock);
    spur_pmix_session_t *session = find_session(plan->namespace_);
    if (session == NULL || session->modex == NULL) {
        if (errbuf != NULL && errlen > 0) {
            snprintf(
                errbuf,
                errlen,
                "PMIx namespace %s is not prepared for peer verification",
                plan->namespace_
            );
        }
        pthread_mutex_unlock(&g_session_lock);
        return -1;
    }
    spur_modex_session_t *modex = session->modex;
    /* Same lifetime rule as spur_fence_nb: keep modex valid until verify finishes. */
    int rc = spur_modex_verify_peers(modex);
    if (rc != SPUR_MODEX_OK) {
        spur_modex_session_abort(modex);
        if (errbuf != NULL && errlen > 0) {
            snprintf(
                errbuf,
                errlen,
                "PMIx peer verification failed: %s",
                spur_modex_strerror(rc)
            );
        }
        pthread_mutex_unlock(&g_session_lock);
        return -1;
    }
    pthread_mutex_unlock(&g_session_lock);
    return 0;
}

#if PMIX_VERSION_MAJOR < 6
static void spur_free_env(char **env) {
    if (env == NULL) {
        return;
    }
    for (char **cur = env; *cur != NULL; cur++) {
        free(*cur);
    }
    free(env);
}

static char **spur_dup_env(char **env) {
    if (env == NULL) {
        return NULL;
    }
    size_t count = 0;
    for (char **cur = env; *cur != NULL; cur++) {
        count++;
    }
    char **out = calloc(count + 1, sizeof(char *));
    if (out == NULL) {
        return NULL;
    }
    for (size_t i = 0; i < count; i++) {
        out[i] = strdup(env[i]);
        if (out[i] == NULL) {
            spur_free_env(out);
            return NULL;
        }
    }
    out[count] = NULL;
    return out;
}
#else
static void spur_free_env(char **env) {
    if (env == NULL) {
        return;
    }
    for (char **cur = env; *cur != NULL; cur++) {
        free(*cur);
    }
    free(env);
}

static char **spur_info_to_env(pmix_info_t *info, size_t ninfo) {
    size_t count = 0;
    for (size_t i = 0; i < ninfo; i++) {
        if (info[i].value.type == PMIX_STRING && info[i].value.data.string != NULL) {
            count++;
        }
    }
    char **out = calloc(count + 1, sizeof(char *));
    if (out == NULL) {
        return NULL;
    }
    size_t j = 0;
    for (size_t i = 0; i < ninfo; i++) {
        if (info[i].value.type != PMIX_STRING || info[i].value.data.string == NULL) {
            continue;
        }
        size_t len = strlen(info[i].key) + 1 + strlen(info[i].value.data.string) + 1;
        out[j] = malloc(len);
        if (out[j] == NULL) {
            spur_free_env(out);
            return NULL;
        }
        snprintf(out[j], len, "%s=%s", info[i].key, info[i].value.data.string);
        j++;
    }
    out[j] = NULL;
    return out;
}
#endif

static char **spur_setup_fork_env_for_rank(const spur_mpi_launch_plan_t *plan, uint32_t rank) {
    pmix_proc_t proc;
    PMIX_LOAD_PROCID(&proc, plan->namespace_, rank);

#if PMIX_VERSION_MAJOR >= 6
    pmix_info_t *info = NULL;
    size_t ninfo = 0;
    pmix_status_t rc = PMIx_server_setup_fork(&proc, &info, &ninfo);
    if (!pmix_status_ok(rc)) {
        spur_mpi_debug(
            "spur_mpi_pmix: PMIx_server_setup_fork rank=%u failed: %s\n",
            rank,
            PMIx_Error_string(rc)
        );
        return NULL;
    }
    char **env = spur_info_to_env(info, ninfo);
    PMIx_Info_release(info, ninfo);
    return env;
#else
    char **env = NULL;
    pmix_status_t rc = PMIx_server_setup_fork(&proc, &env);
    if (!pmix_status_ok(rc)) {
        spur_mpi_debug(
            "spur_mpi_pmix: PMIx_server_setup_fork rank=%u failed: %s\n",
            rank,
            PMIx_Error_string(rc)
        );
        return NULL;
    }
    char **dup = spur_dup_env(env);
    spur_free_env(env);
    return dup;
#endif
}

int spur_mpi_pmix_setup_fork_env(
    const spur_mpi_launch_plan_t *plan,
    uint32_t rank,
    char ***env_out
) {
    if (plan == NULL || env_out == NULL) {
        return -1;
    }
    *env_out = NULL;

    char **env = spur_setup_fork_env_for_rank(plan, rank);
    if (env == NULL) {
        return -1;
    }
    *env_out = env;
    return 0;
}

void spur_mpi_pmix_setup_fork_env_free(char **env) {
    spur_free_env(env);
}
