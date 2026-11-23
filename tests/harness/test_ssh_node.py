# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for SshNode's reconnect, keepalive, and timeout handling.

The fakes stand in for paramiko only; every test drives the real SshNode.
"""

import socket
import threading

import paramiko
import pytest

from cluster import SshConnectionLost, SshNode

HOST = "node-a"


class FakeClock:
    def __init__(self):
        self.now = 0.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float):
        self.now += seconds


class FakeChannel:
    """Plays back one command: stdout/stderr chunks, then how it ends.

    *ending* is "exit" (EOF plus exit status), "hang" (never finishes), "drop"
    (the transport dies after the command was sent), or "no_status" (EOF with
    no exit status on a live transport). *exec_error* and *recv_error* are
    raised from those calls after killing the transport."""

    def __init__(self, remote, transport, stdout=(), stderr=(), exit_status=0,
                 ending="exit", hang_polls=0, exec_error=None, recv_error=None):
        self.remote = remote
        self.transport = transport
        self.stdout = list(stdout)
        self.stderr = list(stderr)
        self.exit_status = exit_status
        self.ending = ending
        self.hang_polls = hang_polls
        self.exec_error = exec_error
        self.recv_error = recv_error
        self.timeout = None
        self.closed = False

    def exec_command(self, cmd):
        self.remote.executed.append((self.remote.clients.index(self.transport.client), cmd))
        if self.exec_error is not None:
            self.transport.active = False
            raise self.exec_error

    def settimeout(self, timeout):
        self.timeout = timeout

    def _block(self):
        self.remote.clock.advance(self.timeout)
        raise socket.timeout()

    def recv_ready(self):
        return bool(self.stdout)

    def recv_stderr_ready(self):
        return bool(self.stderr)

    def recv(self, nbytes):
        if self.recv_error is not None and not self.stdout:
            self.transport.active = False
            raise self.recv_error
        if self.hang_polls:
            self.hang_polls -= 1
            self._block()
        if self.stdout:
            return self.stdout.pop(0)
        if self.ending == "hang":
            self._block()
        if self.ending == "drop":
            self.transport.active = False
        return b""

    def recv_stderr(self, nbytes):
        return self.stderr.pop(0) if self.stderr else b""

    def recv_exit_status(self):
        return self.exit_status if self.ending == "exit" else -1

    def close(self):
        self.closed = True


class FakeTransport:
    def __init__(self, remote, client):
        self.remote = remote
        self.client = client
        self.active = True
        self.keepalive = None
        self.fail_open = False

    def is_active(self):
        return self.active

    def set_keepalive(self, interval):
        self.keepalive = interval

    def open_session(self, timeout=None):
        if self.fail_open:
            self.active = False
            raise paramiko.SSHException("transport died opening a channel")
        channel = FakeChannel(self.remote, self, **self.remote.behaviour)
        self.remote.channels.append(channel)
        return channel


class FakeClient:
    def __init__(self, remote):
        self.remote = remote
        self.transport = None
        self.closed = False
        self.sftp_opened = 0

    def set_missing_host_key_policy(self, policy):
        pass

    def connect(self, **kwargs):
        self.remote.connects.append(kwargs)
        if self.remote.connect_error is not None:
            raise self.remote.connect_error
        self.transport = FakeTransport(self.remote, self)

    def get_transport(self):
        return self.transport

    def open_sftp(self):
        self.sftp_opened += 1
        return object()

    def close(self):
        self.closed = True
        if self.transport is not None:
            self.transport.active = False


class FakeRemote:
    """Shared world every fake client of one node talks to."""

    def __init__(self):
        self.clock = FakeClock()
        self.clients = []
        self.channels = []
        self.connects = []
        self.executed = []
        self.sleeps = []
        self.connect_error = None
        self.behaviour = {"stdout": [b"ok\n"]}

    def new_client(self):
        client = FakeClient(self)
        self.clients.append(client)
        return client


@pytest.fixture
def remote():
    return FakeRemote()


@pytest.fixture
def node(remote):
    ssh = SshNode(HOST, "vm", password="vm", client_factory=remote.new_client)
    ssh._sleep = remote.sleeps.append
    ssh._clock = remote.clock
    return ssh


def test_reconnects_when_transport_inactive_before_send(remote, node):
    remote.clients[0].transport.active = False
    assert not node.is_alive()

    assert node.exec("hostname") == "ok\n"

    assert len(remote.clients) == 2
    assert node.client is remote.clients[1]
    assert remote.clients[0].closed
    assert remote.executed == [(1, "hostname")]
    assert node.is_alive()


def test_reconnects_when_channel_open_finds_transport_dead(remote, node):
    remote.clients[0].transport.fail_open = True

    assert node.exec("hostname") == "ok\n"

    assert len(remote.clients) == 2
    assert remote.executed == [(1, "hostname")]


def test_does_not_rerun_command_when_channel_drops_midway(remote, node):
    remote.behaviour = {"stdout": [b"half"], "ending": "drop"}

    with pytest.raises(SshConnectionLost, match=HOST):
        node.exec("long-job")

    assert remote.executed == [(0, "long-job")]
    assert len(remote.clients) == 1


class ArrivalCountingLock:
    """A real lock that counts callers reaching it, so a test can hold the
    first reconnect until every other caller is queued behind it."""

    def __init__(self, cond):
        self._lock = threading.Lock()
        self._cond = cond
        self.arrivals = 0

    def __enter__(self):
        with self._cond:
            self.arrivals += 1
            self._cond.notify_all()
        self._lock.acquire()

    def __exit__(self, *exc):
        self._lock.release()


def test_concurrent_callers_reconnect_once(remote, node):
    callers = 6
    remote.clients[0].transport.active = False
    cond = threading.Condition()
    lock = ArrivalCountingLock(cond)
    node._lock = lock

    # The first reconnect waits here until all callers are queued on the lock,
    # or until a second caller also starts reconnecting, which fails the test.
    def sleep(delay):
        with cond:
            remote.sleeps.append(delay)
            cond.notify_all()
            cond.wait_for(lambda: lock.arrivals >= callers or len(remote.sleeps) > 1)

    node._sleep = sleep
    results, errors = [], []

    def call(i):
        try:
            results.append(node.exec(f"echo {i}"))
        except Exception as err:
            errors.append(err)

    threads = [threading.Thread(target=call, args=(i,)) for i in range(callers)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == []
    assert results == ["ok\n"] * callers
    assert remote.sleeps == [1]
    assert len(remote.connects) == 2
    assert sorted(remote.executed) == [(1, f"echo {i}") for i in range(callers)]


def test_reconnect_gives_up_after_3_attempts(remote, node):
    remote.clients[0].transport.active = False
    remote.connect_error = OSError("connection refused")

    with pytest.raises(SshConnectionLost, match=HOST):
        node.exec("hostname")

    assert remote.sleeps == [1, 2, 4]
    assert len(remote.connects) == 1 + 3
    assert remote.executed == []


@pytest.mark.parametrize("error", [
    paramiko.AuthenticationException("bad credentials"),
    paramiko.BadHostKeyException(HOST, paramiko.RSAKey.generate(1024), paramiko.RSAKey.generate(1024)),
], ids=["auth", "host-key"])
def test_reconnect_fails_fast_on_permanent_errors(remote, node, error):
    remote.clients[0].transport.active = False
    remote.connect_error = error

    with pytest.raises(SshConnectionLost, match=HOST) as raised:
        node.exec("hostname")

    assert raised.value.__cause__ is error
    assert remote.sleeps == [1]
    assert len(remote.connects) == 1 + 1
    assert remote.executed == []


def test_exec_command_failure_on_dead_transport_is_not_retried(remote, node):
    remote.behaviour = {"exec_error": paramiko.SSHException("transport died")}

    with pytest.raises(SshConnectionLost, match=HOST):
        node.exec("long-job")

    assert remote.executed == [(0, "long-job")]
    assert len(remote.clients) == 1


@pytest.mark.parametrize("error", [
    OSError("connection reset"), EOFError(), paramiko.SSHException("transport died"),
], ids=["oserror", "eof", "ssh"])
def test_read_failure_on_dead_transport_raises_connection_lost(remote, node, error):
    remote.behaviour = {"stdout": [b"half"], "recv_error": error}

    with pytest.raises(SshConnectionLost, match=HOST) as raised:
        node.exec("long-job")

    assert raised.value.__cause__ is error
    assert remote.executed == [(0, "long-job")]
    assert len(remote.clients) == 1
    assert remote.channels[0].closed


def test_missing_exit_status_on_live_transport_returns_minus_one(remote, node):
    remote.behaviour = {"stdout": [b"out"], "ending": "no_status"}

    assert node._run("cmd") == (-1, "out", "")
    assert node.is_alive()


def test_default_timeout_returns_minus_one_and_drains_output(remote, node):
    remote.behaviour = {"stdout": [b"partial "], "stderr": [b"warn"], "ending": "hang"}

    code, out = node.exec_with_exit("stuck")

    assert code == -1
    assert out == "partial warn"
    assert remote.clock.now == pytest.approx(SshNode.DEFAULT_TIMEOUT_S)
    assert remote.channels[0].closed


def test_explicit_timeout_bounds_the_wait(remote, node):
    remote.behaviour = {"ending": "hang"}

    code, _ = node.exec_with_exit("stuck", timeout=30)

    assert code == -1
    assert remote.clock.now == pytest.approx(30)


def test_unbounded_timeout_waits_for_exit(remote, node):
    remote.behaviour = {"stdout": [b"done"], "exit_status": 0,
                        "hang_polls": 2 * SshNode.DEFAULT_TIMEOUT_S}

    code, out = node.exec_with_exit("slow", timeout=None)

    assert (code, out) == (0, "done")
    assert remote.clock.now > SshNode.DEFAULT_TIMEOUT_S


def test_run_returns_exit_code_and_split_streams(remote, node):
    remote.behaviour = {"stdout": [b"out-a", b"out-b"], "stderr": [b"err"], "exit_status": 3}

    assert node._run("cmd") == (3, "out-aout-b", "err")
    assert remote.channels[0].closed


def test_exec_raises_on_timeout(remote, node):
    remote.behaviour = {"ending": "hang"}

    with pytest.raises(RuntimeError, match="exit -1"):
        node.exec("stuck", timeout=5)


def test_keepalive_set_on_connect(remote, node):
    connect = remote.connects[0]
    assert connect["timeout"] == 15
    assert connect["banner_timeout"] == 30
    assert connect["auth_timeout"] == 30
    assert remote.clients[0].transport.keepalive == 15


def test_reconnect_resets_sftp_and_keeps_alive(remote, node):
    first = node.sftp
    remote.clients[0].transport.active = False

    second = node.sftp

    assert second is not first
    assert remote.clients[1].sftp_opened == 1
    assert remote.clients[1].transport.keepalive == 15
    assert remote.connects[1]["timeout"] == 15
