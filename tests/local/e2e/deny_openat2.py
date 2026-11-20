# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Install a child-only openat2 fault before execing a real daemon."""

import ctypes
import errno
import os
import platform
import sys


class SockFilter(ctypes.Structure):
    _fields_ = [("code", ctypes.c_ushort), ("jt", ctypes.c_ubyte),
                ("jf", ctypes.c_ubyte), ("k", ctypes.c_uint32)]


class SockFprog(ctypes.Structure):
    _fields_ = [("length", ctypes.c_ushort), ("filter", ctypes.POINTER(SockFilter))]


class FaultUnavailable(RuntimeError):
    pass


def deny_openat2():
    architectures = {"x86_64": 0xC000003E, "aarch64": 0xC00000B7}
    architecture = architectures.get(platform.machine())
    if architecture is None or ctypes.sizeof(ctypes.c_void_p) != 8:
        raise FaultUnavailable("openat2 fault fixture requires native x86_64 or aarch64")
    if len(os.listdir("/proc/self/task")) != 1:
        raise RuntimeError("seccomp launcher must be single-threaded")
    instructions = (SockFilter * 6)(
        SockFilter(0x20, 0, 0, 4),
        SockFilter(0x15, 0, 3, architecture),
        SockFilter(0x20, 0, 0, 0),
        SockFilter(0x15, 0, 1, 437),
        SockFilter(0x06, 0, 0, 0x00050000 | errno.ENOSYS),
        SockFilter(0x06, 0, 0, 0x7FFF0000),
    )
    program = SockFprog(len(instructions), instructions)
    libc = ctypes.CDLL(None, use_errno=True)
    libc.prctl.argtypes = [ctypes.c_int, *([ctypes.c_ulong] * 4)]
    if libc.prctl(38, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "PR_SET_NO_NEW_PRIVS")
    if libc.prctl(22, 2, ctypes.addressof(program), 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "PR_SET_SECCOMP")
    libc.syscall.restype = ctypes.c_long
    ctypes.set_errno(0)
    result = libc.syscall(437, -1, ctypes.c_char_p(b""), ctypes.c_void_p(), 0)
    if result != -1 or ctypes.get_errno() != errno.ENOSYS:
        raise RuntimeError("installed seccomp filter did not deny openat2 with ENOSYS")


def main():
    probe = sys.argv[1:] == ["--probe"]
    try:
        deny_openat2()
    except (OSError, RuntimeError) as error:
        unavailable = isinstance(error, FaultUnavailable) or (
            isinstance(error, OSError) and error.errno in (errno.EPERM, errno.EACCES, errno.EINVAL, errno.ENOSYS)
        )
        print(f"openat2 fault setup failed: {error}", file=sys.stderr, flush=True)
        return 77 if probe and unavailable else 1
    if probe:
        return 0
    print("fixture: openat2 returns ENOSYS", file=sys.stderr, flush=True)
    os.execv(sys.argv[1], sys.argv[1:])


if __name__ == "__main__":
    sys.exit(main())
