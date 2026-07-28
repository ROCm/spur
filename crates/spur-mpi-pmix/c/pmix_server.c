// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <pmix.h>
#include <pmix_server.h>

#include "spur_mpi_plugin.h"

#define SPUR_PMIX_MAX_SESSIONS 64

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
        cbfunc(PMIX_SUCCESS, NULL, 0, cbdata, NULL, NULL);
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
        cbfunc(PMIX_SUCCESS, NULL, 0, cbdata, NULL, NULL);
    }
    return PMIX_SUCCESS;
}

static pmix_status_t spur_fence(
    const pmix_proc_t *proc,
    const pmix_info_t info[],
    size_t ninfo,
    const pmix_proc_t procs[],
    size_t nprocs,
    const pmix_info_t directives[],
    size_t ndirs,
    pmix_modex_cbfunc_t cbfunc,
    void *cbdata
) {
    (void)proc;
    (void)info;
    (void)ninfo;
    (void)procs;
    (void)nprocs;
    (void)directives;
    (void)ndirs;
    /* Single-node only; multi-node must synchronize here. */
    if (cbfunc != NULL) {
        cbfunc(PMIX_SUCCESS, NULL, 0, cbdata, NULL, NULL);
    }
    return PMIX_SUCCESS;
}

static pmix_server_module_t spur_module = {
    .client_connected = spur_client_connected,
    .client_finalized = spur_client_finalized,
    .fence = spur_fence,
};

static int ensure_server_init(char *errbuf, size_t errlen) {
    if (g_server_initialized) {
        return 0;
    }
    pmix_status_t rc = PMIx_server_init(&spur_module, NULL, 0);
    if (rc != PMIX_SUCCESS) {
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

    pmix_proc_t *procs = calloc(plan->num_local_procs, sizeof(pmix_proc_t));
    if (procs == NULL) {
        if (errbuf != NULL && errlen > 0) {
            snprintf(errbuf, errlen, "failed to allocate PMIx proc table");
        }
        pthread_mutex_unlock(&g_session_lock);
        return -1;
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

    if (rc != PMIX_SUCCESS) {
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

    spur_pmix_session_t *session = alloc_session(plan->namespace_);
    if (session == NULL) {
        pmix_status_t dereg = PMIx_server_deregister_nspace(plan->namespace_);
        if (errbuf != NULL && errlen > 0) {
            if (dereg != PMIX_SUCCESS) {
                snprintf(
                    errbuf,
                    errlen,
                    "PMIx session table full and deregister failed: %s",
                    PMIx_Error_string(dereg)
                );
            } else {
                snprintf(errbuf, errlen, "PMIx session table full");
            }
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
        pmix_status_t rc = PMIx_server_deregister_nspace(namespace_);
        if (rc != PMIX_SUCCESS) {
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

    pmix_proc_t proc;
    PMIX_LOAD_PROCID(&proc, plan->namespace_, rank);

    pmix_info_t *info = NULL;
    size_t ninfo = 0;
    pmix_status_t rc = PMIx_server_setup_fork(&proc, &info, &ninfo);
    if (rc != PMIX_SUCCESS) {
        spur_mpi_debug(
            "spur_mpi_pmix: PMIx_server_setup_fork rank=%u key=%s failed: %s\n",
            rank,
            key,
            PMIx_Error_string(rc)
        );
        return -1;
    }

    int found = -1;
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
    if (found != 0) {
        spur_mpi_debug(
            "spur_mpi_pmix: PMIx env key %s not found for rank=%u\n",
            key,
            rank
        );
    }
    return found;
}
