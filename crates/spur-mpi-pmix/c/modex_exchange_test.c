// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#include "modex_exchange.h"

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

static char g_peer_hosts[2][256];

#define SPUR_TEST_STEP_ID 0xFFFFFFFEu
#define SPUR_TEST_MAGIC 0x53505552u
#define SPUR_TEST_VERSION 3u
#define SPUR_TEST_FLAG_ABORT 0x00000001u

/* Declared here rather than shared with modex_exchange.c so the tests pin the
 * wire layout from outside; a silent field reorder there must fail here. */
typedef struct __attribute__((packed)) {
    uint32_t magic;
    uint32_t version;
    uint32_t job_id;
    uint32_t step_id;
    uint32_t node_index;
    uint32_t fence_seq;
    uint32_t data_len;
    uint32_t flags;
} test_hdr_t;

typedef struct __attribute__((packed)) {
    uint32_t magic;
    uint32_t version;
    uint32_t job_id;
    uint32_t node_index;
    uint32_t fence_seq;
    uint32_t data_len;
    uint32_t flags;
} test_hdr_v2_t;

_Static_assert(sizeof(test_hdr_t) == 32, "v3 modex frame header must stay 32 bytes");
_Static_assert(sizeof(test_hdr_v2_t) == 28, "v2 modex frame header was 28 bytes");

static spur_modex_session_t *make_session(uint32_t job_id) {
    return spur_modex_session_create(job_id, SPUR_TEST_STEP_ID, 2, 0, g_peer_hosts, 2, NULL);
}

static const spur_modex_timeouts_t g_fast_timeouts = {
    .connect_sec = 1,
    .fence_sec = 1,
    .verify_sec = 1,
};

static spur_modex_session_t *make_listener(uint32_t job_id, uint32_t step_id, uint32_t node_index) {
    return spur_modex_session_create(
        job_id,
        step_id,
        2,
        node_index,
        g_peer_hosts,
        2,
        &g_fast_timeouts
    );
}

static int connect_local(uint16_t port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        return -1;
    }
    /* Not a timing assumption: every server path either acks or closes, so this
     * only converts a future never-answers regression into a failure not a hang. */
    struct timeval tv = {.tv_sec = 10, .tv_usec = 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    addr.sin_addr.s_addr = inet_addr("127.0.0.1");
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

/* 0 = server answered with a frame, 1 = server hung up, -1 = neither. */
static int send_frame_and_read_reply(uint16_t port, const void *frame, size_t len) {
    int fd = connect_local(port);
    if (fd < 0) {
        fprintf(stderr, "connect to modex port %u failed: %s\n", port, strerror(errno));
        return -1;
    }
    if (send(fd, frame, len, 0) != (ssize_t)len) {
        fprintf(stderr, "short send to modex port %u: %s\n", port, strerror(errno));
        close(fd);
        return -1;
    }
    char reply[sizeof(test_hdr_t)];
    ssize_t got = recv(fd, reply, sizeof(reply), 0);
    int err = errno;
    close(fd);
    if (got > 0) {
        return 0;
    }
    if (got == 0 || (err != EAGAIN && err != EWOULDBLOCK)) {
        return 1;
    }
    fprintf(stderr, "server on port %u neither acked nor hung up\n", port);
    return -1;
}

/* The accept loop is single-threaded and the accept queue is FIFO, so an ack for
 * a frame sent last proves every earlier frame has already been handled. */
static int drain_accept_queue(uint16_t port, uint32_t job_id, uint32_t step_id) {
    test_hdr_t barrier = {
        .magic = SPUR_TEST_MAGIC,
        .version = SPUR_TEST_VERSION,
        .job_id = job_id,
        .step_id = step_id,
        .node_index = SPUR_MODEX_MAX_NODES,
        .fence_seq = 0,
        .data_len = 0,
        .flags = 0,
    };
    if (send_frame_and_read_reply(port, &barrier, sizeof(barrier)) != 0) {
        fprintf(stderr, "barrier frame for job %u step %u was not acked\n", job_id, step_id);
        return 1;
    }
    return 0;
}

static uint32_t colliding_step_for(uint32_t job_a, uint32_t step_a, uint32_t job_b) {
    uint16_t target = spur_modex_port_for_step(job_a, step_a);
    for (uint32_t step = 0; step < SPUR_MODEX_PORT_SPAN; step++) {
        if (spur_modex_port_for_step(job_b, step) == target) {
            return step;
        }
    }
    return UINT32_MAX;
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

/* Pinned against spur_core::mpi::modex_port_for_step's own test vectors: the two
 * derivations are a cross-node wire contract and drift is silent. */
static int test_port_matches_rust_derivation(void) {
    struct {
        uint32_t job_id;
        uint32_t step_id;
        uint16_t port;
    } cases[] = {
        {0, 0, 16819},
        {1, 0, 20580},
        {42, 0, 24381},
        {42, 0xFFFFFFFEu, 24379},
    };
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        uint16_t got = spur_modex_port_for_step(cases[i].job_id, cases[i].step_id);
        if (got != cases[i].port) {
            fprintf(
                stderr,
                "port for job %u step %u was %u, expected %u\n",
                cases[i].job_id,
                cases[i].step_id,
                got,
                cases[i].port
            );
            return 1;
        }
    }
    return 0;
}

/* Each case drives the wire sequence a real peer produces: push_local_blob emits
 * a data frame and waits for the ack, session_abort emits a lone abort frame. */
struct frame_case {
    const char *name;
    uint32_t job_id;
    uint32_t step_id;
    uint32_t flags;
    bool expect_ack;
    bool expect_abort;
};

static int run_frame_case(const struct frame_case *tc) {
    const uint32_t job_id = 5150;
    const uint32_t step_id = 3;
    spur_modex_session_t *victim = make_listener(job_id, step_id, 0);
    if (victim == NULL) {
        fprintf(stderr, "[%s] modex create failed\n", tc->name);
        return 1;
    }
    if (spur_modex_session_start(victim) != SPUR_MODEX_OK) {
        fprintf(stderr, "[%s] modex start failed\n", tc->name);
        spur_modex_session_release(victim);
        return 1;
    }
    uint16_t port = spur_modex_port_for_step(job_id, step_id);

    test_hdr_t frame = {
        .magic = SPUR_TEST_MAGIC,
        .version = SPUR_TEST_VERSION,
        .job_id = tc->job_id,
        .step_id = tc->step_id,
        .node_index = 1,
        .fence_seq = 0,
        .data_len = 0,
        .flags = tc->flags,
    };
    int rc = 1;
    int reply = send_frame_and_read_reply(port, &frame, sizeof(frame));
    if (reply < 0) {
        goto done;
    }
    if (tc->expect_ack && reply != 0) {
        fprintf(stderr, "[%s] expected the frame to be accepted and acked\n", tc->name);
        goto done;
    }
    if (!tc->expect_ack && reply == 0) {
        fprintf(stderr, "[%s] expected the frame to be rejected, but it was acked\n", tc->name);
        goto done;
    }
    if (drain_accept_queue(port, job_id, step_id) != 0) {
        goto done;
    }
    bool aborted = spur_modex_session_aborted_for_testing(victim);
    if (aborted != tc->expect_abort) {
        fprintf(
            stderr,
            "[%s] session aborted=%d, expected %d\n",
            tc->name,
            (int)aborted,
            (int)tc->expect_abort
        );
        goto done;
    }
    if (!tc->expect_ack && spur_modex_session_remote_present_for_testing(victim, 1)) {
        fprintf(stderr, "[%s] rejected frame still landed in remote[1]\n", tc->name);
        goto done;
    }
    rc = 0;

done:
    spur_modex_session_release(victim);
    return rc;
}

static int test_frames_are_gated_on_identity(void) {
    const uint32_t job_id = 5150;
    const uint32_t step_id = 3;
    const struct frame_case cases[] = {
        {"foreign job data frame", job_id + 1, step_id, 0, false, false},
        {"foreign job abort frame", job_id + 1, step_id, SPUR_TEST_FLAG_ABORT, false, false},
        {"same job other step data frame", job_id, step_id + 1, 0, false, false},
        {"same job other step abort frame", job_id, step_id + 1, SPUR_TEST_FLAG_ABORT, false, false},
        {"own abort frame", job_id, step_id, SPUR_TEST_FLAG_ABORT, false, true},
        {"own data frame", job_id, step_id, 0, true, false},
    };
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        if (run_frame_case(&cases[i]) != 0) {
            return 1;
        }
    }
    return 0;
}

/* A v2 sender writes a 28-byte header then its payload, so a v3 listener reads
 * 32 bytes that straddle both; it must reject on version, not misparse. */
#define SPUR_TEST_V2_PAYLOAD "modexv2!"

static int test_v2_peer_frame_rejected(void) {
    const uint32_t job_id = 6260;
    const uint32_t step_id = 1;
    spur_modex_session_t *victim = make_listener(job_id, step_id, 0);
    if (victim == NULL || spur_modex_session_start(victim) != SPUR_MODEX_OK) {
        fprintf(stderr, "v2 test: modex listener setup failed\n");
        spur_modex_session_release(victim);
        return 1;
    }
    uint16_t port = spur_modex_port_for_step(job_id, step_id);

    struct __attribute__((packed)) {
        test_hdr_v2_t hdr;
        char payload[8];
    } frame = {
        .hdr = {
            .magic = SPUR_TEST_MAGIC,
            .version = 2u,
            .job_id = job_id,
            /* Sits where v3 reads step_id, so a misparse would clear the identity
             * gate and leave only the version check standing between it and us. */
            .node_index = step_id,
            .fence_seq = 0,
            .data_len = sizeof(SPUR_TEST_V2_PAYLOAD) - 1,
            .flags = 0,
        },
        .payload = SPUR_TEST_V2_PAYLOAD,
    };

    int rc = 1;
    /* Misparsed, the first payload word becomes `flags`; its low bit must read as
     * ABORT or a lenient version check would pass this test for the wrong reason. */
    if ((frame.payload[0] & 1) == 0) {
        fprintf(stderr, "v2 test: payload no longer aliases the abort bit\n");
        goto done;
    }
    if (send_frame_and_read_reply(port, &frame, sizeof(frame)) != 1) {
        fprintf(stderr, "v2 test: expected the v2 frame to be rejected without an ack\n");
        goto done;
    }
    if (drain_accept_queue(port, job_id, step_id) != 0) {
        goto done;
    }
    if (spur_modex_session_aborted_for_testing(victim)) {
        fprintf(stderr, "v2 test: a v2 frame aborted the session\n");
        goto done;
    }
    if (spur_modex_session_remote_present_for_testing(victim, 1)) {
        fprintf(stderr, "v2 test: a v2 frame landed in remote[1]\n");
        goto done;
    }
    rc = 0;

done:
    spur_modex_session_release(victim);
    return rc;
}

/* The whole defect end to end: two live sessions hash onto one port, the loser
 * fails to bind and runs its real fence, and the incumbent must survive intact. */
static int test_port_collision_spares_the_incumbent(void) {
    const uint32_t victim_job = 4242;
    const uint32_t victim_step = 0;
    const uint32_t stranger_job = 9999;
    uint32_t stranger_step = colliding_step_for(victim_job, victim_step, stranger_job);
    if (stranger_step == UINT32_MAX) {
        fprintf(stderr, "collision test: no colliding step for job %u\n", stranger_job);
        return 1;
    }

    spur_modex_session_t *victim = make_listener(victim_job, victim_step, 0);
    if (victim == NULL || spur_modex_session_start(victim) != SPUR_MODEX_OK) {
        fprintf(stderr, "collision test: victim listener setup failed\n");
        spur_modex_session_release(victim);
        return 1;
    }

    int rc = 1;
    char *merged = NULL;
    size_t merged_len = 0;
    spur_modex_session_t *stranger = make_listener(stranger_job, stranger_step, 1);
    if (stranger == NULL) {
        fprintf(stderr, "collision test: stranger create failed\n");
        goto done;
    }
    if (spur_modex_session_start(stranger) == SPUR_MODEX_OK) {
        fprintf(stderr, "collision test: stranger bound a port the victim already holds\n");
        goto done;
    }
    if (spur_modex_fence_collect(stranger, "stranger", 8, &merged, &merged_len)
        == SPUR_MODEX_OK)
    {
        fprintf(stderr, "collision test: stranger fenced against a foreign listener\n");
        goto done;
    }
    if (drain_accept_queue(spur_modex_port_for_step(victim_job, victim_step), victim_job, victim_step)
        != 0)
    {
        goto done;
    }
    if (spur_modex_session_aborted_for_testing(victim)) {
        fprintf(stderr, "collision test: an unrelated job's abort killed the victim\n");
        goto done;
    }
    if (spur_modex_session_remote_present_for_testing(victim, 1)) {
        fprintf(stderr, "collision test: an unrelated job's blob landed in remote[1]\n");
        goto done;
    }
    rc = 0;

done:
    free(merged);
    spur_modex_session_release(stranger);
    spur_modex_session_release(victim);
    return rc;
}

int main(void) {
    /* A rejected frame can be met with a close mid-write; a SIGPIPE would look
     * like a crash rather than the rejection the test is asserting. */
    signal(SIGPIPE, SIG_IGN);
    strncpy(g_peer_hosts[0], "127.0.0.1", sizeof(g_peer_hosts[0]) - 1);
    strncpy(g_peer_hosts[1], "127.0.0.2", sizeof(g_peer_hosts[1]) - 1);

    if (test_server_stop_releases_listener() != 0) {
        return 1;
    }
    if (test_extra_retain_leaks_listener() != 0) {
        return 1;
    }
    if (test_port_matches_rust_derivation() != 0) {
        return 1;
    }
    if (test_frames_are_gated_on_identity() != 0) {
        return 1;
    }
    if (test_v2_peer_frame_rejected() != 0) {
        return 1;
    }
    if (test_port_collision_spares_the_incumbent() != 0) {
        return 1;
    }
    return 0;
}
