// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Native authentication counters for `/metrics/auth`.

use crate::export::{encode_registered, register_counter};
use spur_core::native_metrics::NativeAuthMetricsSnapshot;

/// Encode native auth counters as OpenMetrics 1.0 text.
pub fn encode_auth_metrics(snap: &NativeAuthMetricsSnapshot) -> String {
    encode_registered(|registry| {
        register_counter(
            registry,
            "spur_auth_verify_ok",
            "Native bearer credentials accepted",
            snap.verify_ok,
        );
        register_counter(
            registry,
            "spur_auth_verify_fail",
            "Native bearer credentials rejected",
            snap.verify_fail,
        );
        register_counter(
            registry,
            "spur_auth_replay_reject",
            "Native bearer credentials rejected as nonce replay",
            snap.replay_reject,
        );
        register_counter(
            registry,
            "spur_exec_verify_ok",
            "Job/step execution credentials accepted",
            snap.exec_verify_ok,
        );
        register_counter(
            registry,
            "spur_exec_verify_fail",
            "Job/step execution credentials rejected",
            snap.exec_verify_fail,
        );
        register_counter(
            registry,
            "spur_rbac_role_deny",
            "RPCs denied by role binding",
            snap.role_deny,
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_contains_counter_names() {
        let text = encode_auth_metrics(&NativeAuthMetricsSnapshot {
            verify_ok: 1,
            verify_fail: 2,
            replay_reject: 3,
            exec_verify_ok: 4,
            exec_verify_fail: 5,
            role_deny: 6,
        });
        assert!(text.contains("spur_auth_verify_ok"));
        assert!(text.contains("spur_rbac_role_deny"));
    }
}
