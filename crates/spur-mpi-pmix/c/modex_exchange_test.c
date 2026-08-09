// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#include "modex_exchange.h"

#include <stdio.h>
#include <string.h>

static char g_peer_hosts[2][256];

static spur_modex_session_t *make_session(uint32_t job_id) {
    return spur_modex_session_create(job_id, 2, 0, g_peer_hosts, 2, NULL);
}

static void server_stop_modex_cleanup(spur_modex_session_t *modex) {
    spur_modex_session_abort(modex);
    spur_modex_session_release(modex);
}

static int test_server_stop_releases_listener(void) {
    const uint32_t job_id = 4242;
    spur_modex_session_t *modex = make_session(job_id);
    if (modex == NULL) {
        fprintf(stderr, "modex create failed\n");
        return 1;
    }
    if (spur_modex_session_refs_for_testing(modex) != 1) {
        fprintf(stderr, "expected initial ref count 1\n");
        return 1;
    }
    if (spur_modex_session_start(modex) != SPUR_MODEX_OK) {
        fprintf(stderr, "modex start failed\n");
        return 1;
    }
    if (!spur_modex_session_accept_running_for_testing(modex)) {
        fprintf(stderr, "accept thread not running after start\n");
        return 1;
    }

    server_stop_modex_cleanup(modex);

    modex = make_session(job_id);
    if (modex == NULL) {
        fprintf(stderr, "modex recreate failed after stop cleanup\n");
        return 1;
    }
    if (spur_modex_session_start(modex) != SPUR_MODEX_OK) {
        fprintf(stderr, "modex restart failed; listener likely leaked\n");
        return 1;
    }
    spur_modex_session_destroy(modex);
    return 0;
}

static int test_extra_retain_leaks_listener(void) {
    const uint32_t job_id = 4343;
    spur_modex_session_t *modex = make_session(job_id);
    if (modex == NULL) {
        fprintf(stderr, "modex create failed\n");
        return 1;
    }
    if (spur_modex_session_start(modex) != SPUR_MODEX_OK) {
        fprintf(stderr, "modex start failed\n");
        return 1;
    }

    spur_modex_session_retain(modex);
    spur_modex_session_abort(modex);
    spur_modex_session_release(modex);
    if (spur_modex_session_refs_for_testing(modex) != 1) {
        fprintf(stderr, "expected leaked ref count 1 after extra retain\n");
        spur_modex_session_release(modex);
        return 1;
    }
    if (!spur_modex_session_accept_running_for_testing(modex)) {
        fprintf(stderr, "expected accept thread still running after leaked ref\n");
        spur_modex_session_release(modex);
        return 1;
    }

    spur_modex_session_release(modex);
    return 0;
}

int main(void) {
    strncpy(g_peer_hosts[0], "127.0.0.1", sizeof(g_peer_hosts[0]) - 1);
    strncpy(g_peer_hosts[1], "127.0.0.2", sizeof(g_peer_hosts[1]) - 1);

    if (test_server_stop_releases_listener() != 0) {
        return 1;
    }
    if (test_extra_retain_leaks_listener() != 0) {
        return 1;
    }
    return 0;
}
