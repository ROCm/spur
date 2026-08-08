// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#ifndef SPUR_MODEX_EXCHANGE_H
#define SPUR_MODEX_EXCHANGE_H

#include <stddef.h>
#include <stdint.h>

#define SPUR_MODEX_MAX_NODES 64
#define SPUR_MODEX_MAX_BLOB (16u * 1024u * 1024u)
#define SPUR_MODEX_PORT_BASE 16819
#define SPUR_MODEX_PORT_SPAN 8000

#define SPUR_MODEX_DEFAULT_CONNECT_TIMEOUT_SEC 5
#define SPUR_MODEX_DEFAULT_FENCE_TIMEOUT_SEC 120
#define SPUR_MODEX_DEFAULT_VERIFY_TIMEOUT_SEC 30

#define SPUR_MODEX_OK 0
#define SPUR_MODEX_ERR_PARAM -1
#define SPUR_MODEX_ERR_CONNECT -2
#define SPUR_MODEX_ERR_TIMEOUT -3
#define SPUR_MODEX_ERR_ABORT -4
#define SPUR_MODEX_ERR_BLOB -5
#define SPUR_MODEX_ERR_NOMEM -6
#define SPUR_MODEX_ERR_PROTOCOL -7

typedef struct spur_modex_session spur_modex_session_t;

typedef struct spur_modex_timeouts {
    uint32_t connect_sec;
    uint32_t fence_sec;
    uint32_t verify_sec;
} spur_modex_timeouts_t;

static inline uint16_t spur_modex_port_for_job(uint32_t job_id) {
    return (uint16_t)(SPUR_MODEX_PORT_BASE + (job_id % SPUR_MODEX_PORT_SPAN));
}

const char *spur_modex_strerror(int code);

spur_modex_session_t *spur_modex_session_create(
    uint32_t job_id,
    uint32_t num_nodes,
    uint32_t node_index,
    const char peer_hosts[][256],
    uint32_t num_peer_hosts,
    const spur_modex_timeouts_t *timeouts
);

void spur_modex_session_destroy(spur_modex_session_t *session);

void spur_modex_session_retain(spur_modex_session_t *session);

void spur_modex_session_release(spur_modex_session_t *session);

int spur_modex_session_start(spur_modex_session_t *session);

int spur_modex_verify_peers(spur_modex_session_t *session);

int spur_modex_session_abort(spur_modex_session_t *session);

int spur_modex_fence_collect(
    spur_modex_session_t *session,
    const char *local_data,
    size_t local_len,
    char **out_merged,
    size_t *out_merged_len
);

#endif
