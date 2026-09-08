#!/usr/bin/env python3
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Exercise the production module through Linux-PAM without changing host configuration."""

import ctypes
import os
from pathlib import Path
import socket
import sys
import tempfile
import unittest

PAM_SUCCESS = 0
PAM_SYSTEM_ERR = 4
PAM_PERM_DENIED = 6
PAM_CONV_ERR = 19


class PamMessage(ctypes.Structure):
    _fields_ = [("msg_style", ctypes.c_int), ("msg", ctypes.c_char_p)]


class PamResponse(ctypes.Structure):
    _fields_ = [("resp", ctypes.c_void_p), ("resp_retcode", ctypes.c_int)]


ConversationCallback = ctypes.CFUNCTYPE(
    ctypes.c_int,
    ctypes.c_int,
    ctypes.POINTER(ctypes.POINTER(PamMessage)),
    ctypes.POINTER(ctypes.POINTER(PamResponse)),
    ctypes.c_void_p,
)


class PamConversation(ctypes.Structure):
    _fields_ = [("conv", ConversationCallback), ("appdata_ptr", ctypes.c_void_p)]


@unittest.skipUnless(sys.platform == "linux", "Linux-PAM is required")
@unittest.skipIf(os.geteuid() == 0, "requires a genuinely unprivileged caller")
class ProductionPamTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        root = Path(__file__).resolve().parents[3]
        cls.module = Path(os.environ.get(
            "SPUR_PAM_MODULE", str(root / "target/debug/libpam_spur.so")
        )).resolve(strict=True)
        if any(char.isspace() for char in str(cls.module)):
            raise ValueError("PAM module fixture path must not contain whitespace")
        # RTLD_GLOBAL lets the production module resolve real PAM ABI symbols.
        cls.pam = ctypes.CDLL("libpam.so.0", mode=ctypes.RTLD_GLOBAL)
        cls.pam.pam_start_confdir.argtypes = [
            ctypes.c_char_p, ctypes.c_char_p,
            ctypes.POINTER(PamConversation), ctypes.c_char_p,
            ctypes.POINTER(ctypes.c_void_p),
        ]
        cls.pam.pam_start_confdir.restype = ctypes.c_int
        for name in ("pam_acct_mgmt", "pam_open_session", "pam_close_session", "pam_end"):
            function = getattr(cls.pam, name)
            function.argtypes = [ctypes.c_void_p, ctypes.c_int]
            function.restype = ctypes.c_int
        cls.pam.pam_getenv.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        cls.pam.pam_getenv.restype = ctypes.c_char_p
        cls.pam.pam_get_item.argtypes = [
            ctypes.c_void_p, ctypes.c_int, ctypes.POINTER(ctypes.c_void_p),
        ]
        cls.pam.pam_get_item.restype = ctypes.c_int
        # Eager loading makes missing symbols a failure, not a false denial pass.
        cls.loaded_module = ctypes.CDLL(str(cls.module), mode=os.RTLD_NOW)
        for symbol in ("pam_sm_acct_mgmt", "pam_sm_open_session", "pam_sm_close_session"):
            getattr(cls.loaded_module, symbol)

    def setUp(self):
        self.assertNotEqual(os.geteuid(), 0)
        self.directory = tempfile.TemporaryDirectory(prefix="spur-pam-", dir="/tmp")
        self.addCleanup(self.directory.cleanup)
        self.base = Path(self.directory.name)
        self.socket_path = self.base / "agent.sock"
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(self.listener.close)
        self.listener.bind(str(self.socket_path))
        self.listener.listen(8)
        self.listener.setblocking(False)
        self.conversation_calls = []

        def reject_conversation(count, messages, response, appdata):
            self.conversation_calls.append(count)
            return PAM_CONV_ERR

        self.callback = ConversationCallback(reject_conversation)
        self.conversation = PamConversation(self.callback, None)

    def start(self, options):
        (self.base / "sshd").write_text(
            f"account required {self.module} {options}\n"
            f"session required {self.module} {options}\n"
        )
        handle = ctypes.c_void_p()
        status = self.pam.pam_start_confdir(
            b"sshd", b"spur_fixture", ctypes.byref(self.conversation),
            os.fsencode(self.base), ctypes.byref(handle),
        )
        self.assertEqual(status, PAM_SUCCESS, "temporary PAM service must initialize")
        self.assertTrue(handle.value)
        self.addCleanup(self.end, handle)
        service = ctypes.c_void_p()
        self.assertEqual(self.pam.pam_get_item(handle, 1, ctypes.byref(service)), PAM_SUCCESS)
        self.assertEqual(ctypes.string_at(service), b"sshd")
        return handle

    def end(self, handle):
        self.assertEqual(self.pam.pam_end(handle, PAM_SUCCESS), PAM_SUCCESS)

    def assert_no_side_effects(self, handle):
        self.assertEqual(self.conversation_calls, [])
        for key in (b"SPUR_JOB_ID", b"SLURM_JOB_ID", b"ROCR_VISIBLE_DEVICES",
                    b"CUDA_VISIBLE_DEVICES", b"GPU_DEVICE_ORDINAL"):
            self.assertIsNone(self.pam.pam_getenv(handle, key), key)
        try:
            connection, _ = self.listener.accept()
        except BlockingIOError:
            pass
        else:
            connection.close()
            self.fail("unprivileged caller contacted the mock agent")

    def test_account_rejects_unprivileged_caller(self):
        handle = self.start(f"socket={self.socket_path}")
        self.assertEqual(self.pam.pam_acct_mgmt(handle, 0), PAM_PERM_DENIED)
        self.assert_no_side_effects(handle)

    def test_open_session_rejects_even_without_account_call(self):
        handle = self.start(f"socket={self.socket_path}")
        self.assertEqual(self.pam.pam_open_session(handle, 0), PAM_PERM_DENIED)
        self.assert_no_side_effects(handle)

    def test_close_session_rejects_unprivileged_caller(self):
        handle = self.start(f"socket={self.socket_path}")
        self.assertEqual(self.pam.pam_close_session(handle, 0), PAM_PERM_DENIED)
        self.assert_no_side_effects(handle)

    def test_account_then_session_both_fail_closed(self):
        handle = self.start(f"socket={self.socket_path}")
        for operation in (self.pam.pam_acct_mgmt, self.pam.pam_open_session):
            self.assertEqual(operation(handle, 0), PAM_PERM_DENIED)
        self.assert_no_side_effects(handle)

    def test_missing_option_is_module_system_error(self):
        # Distinguish execution of this module from PAM's generic load failure.
        handle = self.start("")
        for operation in (self.pam.pam_acct_mgmt, self.pam.pam_open_session,
                          self.pam.pam_close_session):
            self.assertEqual(operation(handle, 0), PAM_SYSTEM_ERR)
        self.assert_no_side_effects(handle)


if __name__ == "__main__":
    unittest.main(verbosity=2)
