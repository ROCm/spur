// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <pmix.h>
#include <pmix_server.h>
#include <pmix_version.h>

#include "spur_mpi_plugin.h"

#define SPUR_PMIX_MAX_SESSIONS 64

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
 * Single-node jobs rely on the PMIx server's internal fence/modex path (GDS).
 * Overriding fence[_nb] with an empty modex blob prevents clients from exchanging
 * data and breaks PMIx_Get(PMIX_JOB_SIZE). Multi-node will need a real fence here.
 */
static pmix_server_module_t spur_module = {
    .client_connected = spur_client_connected,
    .client_finalized = spur_client_finalized,
};

static int ensure_server_init(char *errbuf, size_t errlen) {
    if (g_server_initialized) {
        return 0;
    }
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
            g_sessions[i].active = 1;
            strncpy(g_sessions[i].namespace_, namespace_, sizeof(g_sessions[i].namespace_) - 1);
            g_sessions[i].namespace_[sizeof(g_sessions[i].namespace_) - 1] = '\0';
            return &g_sessions[i];
        }
    }
    return NULL;
}

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
    pmix_info_t info[2];
    uint32_t local_size = plan->num_local_procs;
    PMIX_INFO_LOAD(&info[0], PMIX_JOB_SIZE, &plan->universe_size, PMIX_UINT32);
    PMIX_INFO_LOAD(&info[1], PMIX_LOCAL_SIZE, &local_size, PMIX_UINT32);
    return PMIx_server_register_nspace(
        plan->namespace_,
        (int)plan->num_local_procs,
        info,
        2,
        NULL,
        NULL
    );
#endif
}

static void spur_deregister_nspace(const char *namespace_) {
#if PMIX_VERSION_MAJOR >= 6
    (void)PMIx_server_deregister_nspace(namespace_);
#else
    PMIx_server_deregister_nspace(namespace_, NULL, NULL);
#endif
}

static pmix_status_t spur_register_clients(const spur_mpi_launch_plan_t *plan) {
    uid_t uid = getuid();
    gid_t gid = getgid();
    for (uint32_t i = 0; i < plan->num_local_procs; i++) {
        pmix_proc_t proc;
        PMIX_LOAD_PROCID(&proc, plan->namespace_, plan->local_procs[i].rank);
        pmix_status_t rc = PMIx_server_register_client(&proc, uid, gid, NULL, NULL, NULL);
        if (!pmix_status_ok(rc)) {
            return rc;
        }
    }
    return PMIX_SUCCESS;
}

#if PMIX_VERSION_MAJOR < 6
static int spur_copy_server_uri(char **env, char *val, size_t vallen) {
    static const char *candidates[] = {
        "PMIX_SERVER_URI4",
        "PMIX_SERVER_URI41",
        "PMIX_SERVER_URI3",
        "PMIX_SERVER_URI2",
        "PMIX_SERVER_URI",
        NULL,
    };

    for (int i = 0; candidates[i] != NULL; i++) {
        size_t prefix_len = strlen(candidates[i]);
        for (char **cur = env; cur != NULL && *cur != NULL; cur++) {
            if (strncmp(*cur, candidates[i], prefix_len) == 0 && (*cur)[prefix_len] == '=') {
                snprintf(val, vallen, "%s", *cur + prefix_len + 1);
                return 0;
            }
        }
    }
    return -1;
}
#endif

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
    if (ensure_server_init(errbuf, errlen) != 0) {
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
    for (uint32_t i = 0; i < plan->num_local_procs && i < 256; i++) {
        session->local_ranks[i] = plan->local_procs[i].rank;
    }
    strncpy(session->tmpdir, plan->tmpdir, sizeof(session->tmpdir) - 1);
    session->tmpdir[sizeof(session->tmpdir) - 1] = '\0';

    spur_mpi_debug(
        "spur_mpi_pmix: registered namespace %s size=%u local_procs=%u\n",
        plan->namespace_,
        plan->universe_size,
        plan->num_local_procs
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
#endif

int spur_mpi_pmix_env(
    const spur_mpi_launch_plan_t *plan,
    uint32_t rank,
    const char *key,
    char *val,
    size_t vallen
) {
    if (plan == NULL || key == NULL || val == NULL || vallen == 0) {
        return -1;
    }

    if (strcmp(key, "PMIX_NAMESPACE") == 0) {
        snprintf(val, vallen, "%s", plan->namespace_);
        return 0;
    }
    if (strcmp(key, "PMIX_RANK") == 0) {
        snprintf(val, vallen, "%u", rank);
        return 0;
    }
    if (strcmp(key, "PMIX_SIZE") == 0 || strcmp(key, "PMIX_JOB_SIZE") == 0) {
        snprintf(val, vallen, "%u", plan->universe_size);
        return 0;
    }
    if (strcmp(key, "PMIX_SERVER_TMPDIR") == 0) {
        snprintf(val, vallen, "%s", plan->tmpdir);
        return 0;
    }
    /* Open MPI 4.x uses URI4/URI3; same aliases in mpi_plugin.rs and task_launch.rs. */
    if (strcmp(key, "PMIX_SERVER_URI4") == 0 || strcmp(key, "PMIX_SERVER_URI3") == 0) {
        key = "PMIX_SERVER_URI";
    }

    pmix_proc_t proc;
    PMIX_LOAD_PROCID(&proc, plan->namespace_, rank);

    int found = -1;
#if PMIX_VERSION_MAJOR >= 6
    pmix_info_t *info = NULL;
    size_t ninfo = 0;
    pmix_status_t rc = PMIx_server_setup_fork(&proc, &info, &ninfo);
    if (!pmix_status_ok(rc)) {
        spur_mpi_debug(
            "spur_mpi_pmix: PMIx_server_setup_fork rank=%u key=%s failed: %s\n",
            rank,
            key,
            PMIx_Error_string(rc)
        );
        return -1;
    }

    for (size_t i = 0; i < ninfo; i++) {
        if (strcmp(info[i].key, key) == 0) {
            if (info[i].value.type == PMIX_STRING && info[i].value.data.string != NULL) {
                snprintf(val, vallen, "%s", info[i].value.data.string);
                found = 0;
            }
            break;
        }
    }
    PMIx_Info_release(info, ninfo);
#else
    char **env = NULL;
    pmix_status_t rc = PMIx_server_setup_fork(&proc, &env);
    if (!pmix_status_ok(rc)) {
        spur_mpi_debug(
            "spur_mpi_pmix: PMIx_server_setup_fork rank=%u key=%s failed: %s\n",
            rank,
            key,
            PMIx_Error_string(rc)
        );
        return -1;
    }

    size_t key_len = strlen(key);
    if (strcmp(key, "PMIX_SERVER_URI") == 0) {
        found = spur_copy_server_uri(env, val, vallen);
    } else {
        for (char **cur = env; cur != NULL && *cur != NULL; cur++) {
            if (strncmp(*cur, key, key_len) == 0 && (*cur)[key_len] == '=') {
                snprintf(val, vallen, "%s", *cur + key_len + 1);
                found = 0;
                break;
            }
        }
    }
    spur_free_env(env);
#endif

    if (found != 0) {
        spur_mpi_debug(
            "spur_mpi_pmix: PMIx env key %s not found for rank=%u\n",
            key,
            rank
        );
    }
    return found;
}
