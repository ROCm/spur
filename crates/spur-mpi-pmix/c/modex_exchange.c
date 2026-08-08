// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#include "modex_exchange.h"

#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#define SPUR_MODEX_MAGIC 0x53505552u
#define SPUR_MODEX_VERSION 2u
#define SPUR_MODEX_NO_ROUND UINT32_MAX
#define SPUR_MODEX_FLAG_ABORT 0x00000001u
#define SPUR_MODEX_CONNECT_SLEEP_US 100000
#define SPUR_MODEX_CONNECT_RETRIES 300

typedef struct __attribute__((packed)) {
    uint32_t magic;
    uint32_t version;
    uint32_t job_id;
    uint32_t node_index;
    uint32_t fence_seq;
    uint32_t data_len;
    uint32_t flags;
} spur_modex_hdr_t;

struct spur_modex_blob {
    char *data;
    size_t len;
    bool present;
    uint32_t fence_seq;
};

struct spur_modex_session {
    uint32_t job_id;
    uint32_t num_nodes;
    uint32_t node_index;
    uint16_t port;
    char peer_hosts[SPUR_MODEX_MAX_NODES][256];
    struct spur_modex_blob remote[SPUR_MODEX_MAX_NODES];
    int listen_fd;
    pthread_t accept_thread;
    _Atomic bool accept_running;
    bool aborted;
    uint32_t fence_seq;
    uint32_t active_round_seq;
    spur_modex_timeouts_t timeouts;
    pthread_mutex_t lock;
    pthread_cond_t progress;
    _Atomic int refs;
};

const char *spur_modex_strerror(int code) {
    switch (code) {
    case SPUR_MODEX_OK:
        return "success";
    case SPUR_MODEX_ERR_PARAM:
        return "invalid parameter";
    case SPUR_MODEX_ERR_CONNECT:
        return "peer connection failed";
    case SPUR_MODEX_ERR_TIMEOUT:
        return "modex timed out";
    case SPUR_MODEX_ERR_ABORT:
        return "modex aborted by peer";
    case SPUR_MODEX_ERR_BLOB:
        return "invalid modex blob";
    case SPUR_MODEX_ERR_NOMEM:
        return "out of memory";
    case SPUR_MODEX_ERR_PROTOCOL:
        return "modex protocol error";
    default:
        return "unknown modex error";
    }
}

static void normalize_timeouts(spur_modex_timeouts_t *timeouts) {
    if (timeouts->connect_sec == 0) {
        timeouts->connect_sec = SPUR_MODEX_DEFAULT_CONNECT_TIMEOUT_SEC;
    }
    if (timeouts->fence_sec == 0) {
        timeouts->fence_sec = SPUR_MODEX_DEFAULT_FENCE_TIMEOUT_SEC;
    }
    if (timeouts->verify_sec == 0) {
        timeouts->verify_sec = SPUR_MODEX_DEFAULT_VERIFY_TIMEOUT_SEC;
    }
}

static int set_socket_timeouts(int fd, int sec) {
    struct timeval tv;
    tv.tv_sec = sec;
    tv.tv_usec = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) != 0) {
        return -1;
    }
    if (setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv)) != 0) {
        return -1;
    }
    return 0;
}

static int read_full(int fd, void *buf, size_t len) {
    size_t done = 0;
    while (done < len) {
        ssize_t rc = recv(fd, (char *)buf + done, len - done, 0);
        if (rc <= 0) {
            return -1;
        }
        done += (size_t)rc;
    }
    return 0;
}

static int write_full(int fd, const void *buf, size_t len) {
    size_t done = 0;
    while (done < len) {
        ssize_t rc = send(fd, (const char *)buf + done, len - done, 0);
        if (rc <= 0) {
            return -1;
        }
        done += (size_t)rc;
    }
    return 0;
}

static int recv_blob(int fd, spur_modex_hdr_t *hdr, char **payload) {
    if (read_full(fd, hdr, sizeof(*hdr)) != 0) {
        return SPUR_MODEX_ERR_CONNECT;
    }
    if (hdr->magic != SPUR_MODEX_MAGIC || hdr->version != SPUR_MODEX_VERSION) {
        return SPUR_MODEX_ERR_PROTOCOL;
    }
    if ((hdr->flags & SPUR_MODEX_FLAG_ABORT) != 0) {
        return SPUR_MODEX_ERR_ABORT;
    }
    if (hdr->data_len > SPUR_MODEX_MAX_BLOB) {
        return SPUR_MODEX_ERR_BLOB;
    }
    if (hdr->data_len == 0) {
        *payload = NULL;
        return SPUR_MODEX_OK;
    }
    *payload = malloc(hdr->data_len);
    if (*payload == NULL) {
        return SPUR_MODEX_ERR_NOMEM;
    }
    if (read_full(fd, *payload, hdr->data_len) != 0) {
        free(*payload);
        *payload = NULL;
        return SPUR_MODEX_ERR_CONNECT;
    }
    return SPUR_MODEX_OK;
}

static int send_blob(int fd, const spur_modex_hdr_t *hdr, const char *payload) {
    if (write_full(fd, hdr, sizeof(*hdr)) != 0) {
        return SPUR_MODEX_ERR_CONNECT;
    }
    if (hdr->data_len > 0 && payload != NULL) {
        if (write_full(fd, payload, hdr->data_len) != 0) {
            return SPUR_MODEX_ERR_CONNECT;
        }
    }
    return SPUR_MODEX_OK;
}

static void reset_remote_blobs(spur_modex_session_t *session, uint32_t keep_fence_seq) {
    for (uint32_t i = 0; i < session->num_nodes; i++) {
        if (i == session->node_index) {
            continue;
        }
        if (keep_fence_seq != SPUR_MODEX_NO_ROUND
            && session->remote[i].present
            && session->remote[i].fence_seq == keep_fence_seq)
        {
            continue;
        }
        free(session->remote[i].data);
        session->remote[i].data = NULL;
        session->remote[i].len = 0;
        session->remote[i].present = false;
        session->remote[i].fence_seq = SPUR_MODEX_NO_ROUND;
    }
}

static void mark_aborted(spur_modex_session_t *session) {
    pthread_mutex_lock(&session->lock);
    session->aborted = true;
    pthread_cond_broadcast(&session->progress);
    pthread_mutex_unlock(&session->lock);
}

static void store_remote_blob(
    spur_modex_session_t *session,
    uint32_t node_index,
    uint32_t fence_seq,
    char *data,
    size_t len
) {
    if (node_index >= SPUR_MODEX_MAX_NODES || node_index >= session->num_nodes) {
        free(data);
        return;
    }
    pthread_mutex_lock(&session->lock);
    if (session->active_round_seq == SPUR_MODEX_NO_ROUND) {
        if (fence_seq != session->fence_seq) {
            pthread_mutex_unlock(&session->lock);
            free(data);
            return;
        }
        session->active_round_seq = fence_seq;
    } else if (fence_seq != session->active_round_seq) {
        pthread_mutex_unlock(&session->lock);
        free(data);
        return;
    }
    if (session->remote[node_index].present) {
        free(session->remote[node_index].data);
    }
    session->remote[node_index].data = data;
    session->remote[node_index].len = len;
    session->remote[node_index].present = (data != NULL) || len == 0;
    session->remote[node_index].fence_seq = fence_seq;
    pthread_cond_broadcast(&session->progress);
    pthread_mutex_unlock(&session->lock);
}

static void *accept_loop(void *arg) {
    spur_modex_session_t *session = (spur_modex_session_t *)arg;
    while (atomic_load(&session->accept_running)) {
        int client = accept(session->listen_fd, NULL, NULL);
        if (client < 0) {
            if (!atomic_load(&session->accept_running)) {
                break;
            }
            if (errno == EINTR) {
                continue;
            }
            usleep(SPUR_MODEX_CONNECT_SLEEP_US);
            continue;
        }
        set_socket_timeouts(client, (int)session->timeouts.fence_sec);
        spur_modex_hdr_t hdr;
        char *payload = NULL;
        int recv_rc = recv_blob(client, &hdr, &payload);
        if (recv_rc == SPUR_MODEX_ERR_ABORT) {
            mark_aborted(session);
            close(client);
            continue;
        }
        if (recv_rc != SPUR_MODEX_OK) {
            free(payload);
            close(client);
            continue;
        }
        if (hdr.job_id != session->job_id) {
            free(payload);
            close(client);
            continue;
        }
        store_remote_blob(session, hdr.node_index, hdr.fence_seq, payload, hdr.data_len);

        spur_modex_hdr_t ack = {
            .magic = SPUR_MODEX_MAGIC,
            .version = SPUR_MODEX_VERSION,
            .job_id = session->job_id,
            .node_index = session->node_index,
            .fence_seq = hdr.fence_seq,
            .data_len = 0,
            .flags = 0,
        };
        (void)send_blob(client, &ack, NULL);
        close(client);
    }
    return NULL;
}

spur_modex_session_t *spur_modex_session_create(
    uint32_t job_id,
    uint32_t num_nodes,
    uint32_t node_index,
    const char peer_hosts[][256],
    uint32_t num_peer_hosts,
    const spur_modex_timeouts_t *timeouts
) {
    if (num_nodes <= 1 || num_nodes > SPUR_MODEX_MAX_NODES) {
        return NULL;
    }
    if (node_index >= num_nodes) {
        return NULL;
    }
    if (num_peer_hosts != num_nodes) {
        return NULL;
    }

    spur_modex_session_t *session = calloc(1, sizeof(*session));
    if (session == NULL) {
        return NULL;
    }
    session->job_id = job_id;
    session->num_nodes = num_nodes;
    session->node_index = node_index;
    session->port = spur_modex_port_for_job(job_id);
    session->listen_fd = -1;
    atomic_store(&session->accept_running, false);
    session->aborted = false;
    session->fence_seq = 0;
    session->active_round_seq = SPUR_MODEX_NO_ROUND;
    if (timeouts != NULL) {
        session->timeouts = *timeouts;
    }
    normalize_timeouts(&session->timeouts);
    for (uint32_t i = 0; i < num_nodes; i++) {
        strncpy(session->peer_hosts[i], peer_hosts[i], sizeof(session->peer_hosts[i]) - 1);
        session->peer_hosts[i][sizeof(session->peer_hosts[i]) - 1] = '\0';
    }
    pthread_mutex_init(&session->lock, NULL);
    pthread_cond_init(&session->progress, NULL);
    atomic_store(&session->refs, 1);
    return session;
}

void spur_modex_session_retain(spur_modex_session_t *session) {
    if (session != NULL) {
        atomic_fetch_add(&session->refs, 1);
    }
}

static void spur_modex_session_free(spur_modex_session_t *session) {
    if (session == NULL) {
        return;
    }
    if (atomic_load(&session->accept_running)) {
        atomic_store(&session->accept_running, false);
        if (session->listen_fd >= 0) {
            shutdown(session->listen_fd, SHUT_RDWR);
            close(session->listen_fd);
            session->listen_fd = -1;
        }
        pthread_join(session->accept_thread, NULL);
    } else if (session->listen_fd >= 0) {
        close(session->listen_fd);
    }
    for (uint32_t i = 0; i < SPUR_MODEX_MAX_NODES; i++) {
        free(session->remote[i].data);
    }
    pthread_mutex_destroy(&session->lock);
    pthread_cond_destroy(&session->progress);
    free(session);
}

void spur_modex_session_release(spur_modex_session_t *session) {
    if (session == NULL) {
        return;
    }
    if (atomic_fetch_sub(&session->refs, 1) == 1) {
        spur_modex_session_free(session);
    }
}

int spur_modex_session_start(spur_modex_session_t *session) {
    if (session == NULL) {
        return SPUR_MODEX_ERR_PARAM;
    }
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        return SPUR_MODEX_ERR_CONNECT;
    }
    int yes = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &yes, sizeof(yes));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_ANY);
    addr.sin_port = htons(session->port);
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        close(fd);
        return SPUR_MODEX_ERR_CONNECT;
    }
    if (listen(fd, 32) != 0) {
        close(fd);
        return SPUR_MODEX_ERR_CONNECT;
    }
    session->listen_fd = fd;
    atomic_store(&session->accept_running, true);
    if (pthread_create(&session->accept_thread, NULL, accept_loop, session) != 0) {
        atomic_store(&session->accept_running, false);
        close(fd);
        session->listen_fd = -1;
        return SPUR_MODEX_ERR_NOMEM;
    }
    return SPUR_MODEX_OK;
}

void spur_modex_session_destroy(spur_modex_session_t *session) {
    spur_modex_session_release(session);
}

static int try_connect_addr(const struct sockaddr *addr, socklen_t addrlen, int timeout_sec) {
    int fd = socket(addr->sa_family, SOCK_STREAM, 0);
    if (fd < 0) {
        return -1;
    }
    if (set_socket_timeouts(fd, timeout_sec > 0 ? timeout_sec : 1) != 0) {
        close(fd);
        return -1;
    }
    if (connect(fd, addr, addrlen) == 0) {
        return fd;
    }
    close(fd);
    return -1;
}

static int connect_peer(const char *host, uint16_t port, int timeout_sec) {
    int max_attempts = SPUR_MODEX_CONNECT_RETRIES;
    if (timeout_sec > 0) {
        max_attempts = (timeout_sec * 1000000) / SPUR_MODEX_CONNECT_SLEEP_US;
        if (max_attempts < 1) {
            max_attempts = 1;
        }
    }
    char port_str[16];
    snprintf(port_str, sizeof(port_str), "%u", (unsigned)port);

    for (int attempt = 0; attempt < max_attempts; attempt++) {
        struct addrinfo hints;
        memset(&hints, 0, sizeof(hints));
        hints.ai_family = AF_INET;
        hints.ai_socktype = SOCK_STREAM;

        struct addrinfo *res = NULL;
        int gai = getaddrinfo(host, port_str, &hints, &res);
        if (gai != 0) {
            if (attempt + 1 >= max_attempts) {
                return -1;
            }
            usleep(SPUR_MODEX_CONNECT_SLEEP_US);
            continue;
        }

        int fd = -1;
        for (struct addrinfo *cur = res; cur != NULL; cur = cur->ai_next) {
            fd = try_connect_addr(cur->ai_addr, cur->ai_addrlen, timeout_sec);
            if (fd >= 0) {
                break;
            }
        }
        freeaddrinfo(res);
        if (fd >= 0) {
            return fd;
        }
        usleep(SPUR_MODEX_CONNECT_SLEEP_US);
    }
    return -1;
}

int spur_modex_verify_peers(spur_modex_session_t *session) {
    if (session == NULL) {
        return SPUR_MODEX_ERR_PARAM;
    }
    for (uint32_t peer = 0; peer < session->num_nodes; peer++) {
        if (peer == session->node_index) {
            continue;
        }
        int fd = connect_peer(
            session->peer_hosts[peer],
            session->port,
            (int)session->timeouts.verify_sec
        );
        if (fd < 0) {
            return SPUR_MODEX_ERR_CONNECT;
        }
        close(fd);
    }
    return SPUR_MODEX_OK;
}

int spur_modex_session_abort(spur_modex_session_t *session) {
    if (session == NULL) {
        return SPUR_MODEX_ERR_PARAM;
    }
    mark_aborted(session);

    spur_modex_hdr_t abort_hdr = {
        .magic = SPUR_MODEX_MAGIC,
        .version = SPUR_MODEX_VERSION,
        .job_id = session->job_id,
        .node_index = session->node_index,
        .fence_seq = 0,
        .data_len = 0,
        .flags = SPUR_MODEX_FLAG_ABORT,
    };

    for (uint32_t peer = 0; peer < session->num_nodes; peer++) {
        if (peer == session->node_index) {
            continue;
        }
        int fd = connect_peer(
            session->peer_hosts[peer],
            session->port,
            (int)session->timeouts.connect_sec
        );
        if (fd < 0) {
            continue;
        }
        (void)send_blob(fd, &abort_hdr, NULL);
        close(fd);
    }
    return SPUR_MODEX_OK;
}

static int push_local_blob(
    spur_modex_session_t *session,
    uint32_t fence_seq,
    const char *local_data,
    size_t local_len
) {
    spur_modex_hdr_t hdr = {
        .magic = SPUR_MODEX_MAGIC,
        .version = SPUR_MODEX_VERSION,
        .job_id = session->job_id,
        .node_index = session->node_index,
        .fence_seq = fence_seq,
        .data_len = (uint32_t)local_len,
        .flags = 0,
    };
    for (uint32_t peer = 0; peer < session->num_nodes; peer++) {
        if (peer == session->node_index) {
            continue;
        }
        int fd = connect_peer(
            session->peer_hosts[peer],
            session->port,
            (int)session->timeouts.connect_sec
        );
        if (fd < 0) {
            spur_modex_session_abort(session);
            return SPUR_MODEX_ERR_CONNECT;
        }
        if (send_blob(fd, &hdr, local_data) != SPUR_MODEX_OK) {
            close(fd);
            spur_modex_session_abort(session);
            return SPUR_MODEX_ERR_CONNECT;
        }
        spur_modex_hdr_t ack;
        char *ignored = NULL;
        int ack_rc = recv_blob(fd, &ack, &ignored);
        free(ignored);
        close(fd);
        if (ack_rc == SPUR_MODEX_ERR_ABORT) {
            mark_aborted(session);
            return SPUR_MODEX_ERR_ABORT;
        }
        if (ack_rc != SPUR_MODEX_OK) {
            spur_modex_session_abort(session);
            return ack_rc;
        }
    }
    return SPUR_MODEX_OK;
}

static bool all_remotes_present(spur_modex_session_t *session) {
    for (uint32_t i = 0; i < session->num_nodes; i++) {
        if (i == session->node_index) {
            continue;
        }
        if (!session->remote[i].present) {
            return false;
        }
    }
    return true;
}

static int wait_for_remotes(spur_modex_session_t *session) {
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_sec += (time_t)session->timeouts.fence_sec;

    pthread_mutex_lock(&session->lock);
    while (!all_remotes_present(session) && !session->aborted) {
        int rc = pthread_cond_timedwait(&session->progress, &session->lock, &deadline);
        if (rc == ETIMEDOUT) {
            pthread_mutex_unlock(&session->lock);
            spur_modex_session_abort(session);
            return SPUR_MODEX_ERR_TIMEOUT;
        }
        if (rc != 0) {
            pthread_mutex_unlock(&session->lock);
            return SPUR_MODEX_ERR_PROTOCOL;
        }
    }
    bool aborted = session->aborted;
    pthread_mutex_unlock(&session->lock);
    if (aborted) {
        return SPUR_MODEX_ERR_ABORT;
    }
    return SPUR_MODEX_OK;
}

static int merge_blobs(
    spur_modex_session_t *session,
    const char *local_data,
    size_t local_len,
    char **out_merged,
    size_t *out_merged_len
) {
    size_t total = local_len;
    for (uint32_t i = 0; i < session->num_nodes; i++) {
        if (i == session->node_index) {
            continue;
        }
        total += session->remote[i].len;
    }
    char *merged = malloc(total > 0 ? total : 1);
    if (merged == NULL) {
        return SPUR_MODEX_ERR_NOMEM;
    }
    size_t offset = 0;
    for (uint32_t i = 0; i < session->num_nodes; i++) {
        if (i == session->node_index) {
            if (local_len > 0 && local_data != NULL) {
                memcpy(merged + offset, local_data, local_len);
                offset += local_len;
            }
            continue;
        }
        if (session->remote[i].len > 0 && session->remote[i].data != NULL) {
            memcpy(merged + offset, session->remote[i].data, session->remote[i].len);
            offset += session->remote[i].len;
        }
    }
    *out_merged = merged;
    *out_merged_len = offset;
    return SPUR_MODEX_OK;
}

int spur_modex_fence_collect(
    spur_modex_session_t *session,
    const char *local_data,
    size_t local_len,
    char **out_merged,
    size_t *out_merged_len
) {
    if (session == NULL || out_merged == NULL || out_merged_len == NULL) {
        return SPUR_MODEX_ERR_PARAM;
    }
    if (local_len > SPUR_MODEX_MAX_BLOB) {
        return SPUR_MODEX_ERR_BLOB;
    }

    pthread_mutex_lock(&session->lock);
    uint32_t round_seq = session->fence_seq;
    reset_remote_blobs(session, round_seq);
    session->aborted = false;
    session->active_round_seq = round_seq;
    pthread_mutex_unlock(&session->lock);

    int push_rc = push_local_blob(session, round_seq, local_data, local_len);
    if (push_rc != SPUR_MODEX_OK) {
        pthread_mutex_lock(&session->lock);
        session->active_round_seq = SPUR_MODEX_NO_ROUND;
        pthread_mutex_unlock(&session->lock);
        return push_rc;
    }
    int wait_rc = wait_for_remotes(session);
    if (wait_rc != SPUR_MODEX_OK) {
        pthread_mutex_lock(&session->lock);
        session->active_round_seq = SPUR_MODEX_NO_ROUND;
        pthread_mutex_unlock(&session->lock);
        return wait_rc;
    }
    pthread_mutex_lock(&session->lock);
    int merge_rc = merge_blobs(session, local_data, local_len, out_merged, out_merged_len);
    if (merge_rc == SPUR_MODEX_OK) {
        session->fence_seq++;
    }
    session->active_round_seq = SPUR_MODEX_NO_ROUND;
    pthread_mutex_unlock(&session->lock);
    return merge_rc;
}
