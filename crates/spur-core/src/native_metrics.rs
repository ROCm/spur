// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Process-wide native authentication counters for OpenMetrics export.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct NativeAuthMetrics {
    pub verify_ok: AtomicU64,
    pub verify_fail: AtomicU64,
    pub replay_reject: AtomicU64,
    pub exec_verify_ok: AtomicU64,
    pub exec_verify_fail: AtomicU64,
    pub role_deny: AtomicU64,
}

impl NativeAuthMetrics {
    pub fn snapshot(&self) -> NativeAuthMetricsSnapshot {
        NativeAuthMetricsSnapshot {
            verify_ok: self.verify_ok.load(Ordering::Relaxed),
            verify_fail: self.verify_fail.load(Ordering::Relaxed),
            replay_reject: self.replay_reject.load(Ordering::Relaxed),
            exec_verify_ok: self.exec_verify_ok.load(Ordering::Relaxed),
            exec_verify_fail: self.exec_verify_fail.load(Ordering::Relaxed),
            role_deny: self.role_deny.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeAuthMetricsSnapshot {
    pub verify_ok: u64,
    pub verify_fail: u64,
    pub replay_reject: u64,
    pub exec_verify_ok: u64,
    pub exec_verify_fail: u64,
    pub role_deny: u64,
}

static GLOBAL: NativeAuthMetrics = NativeAuthMetrics {
    verify_ok: AtomicU64::new(0),
    verify_fail: AtomicU64::new(0),
    replay_reject: AtomicU64::new(0),
    exec_verify_ok: AtomicU64::new(0),
    exec_verify_fail: AtomicU64::new(0),
    role_deny: AtomicU64::new(0),
};

pub fn global() -> &'static NativeAuthMetrics {
    &GLOBAL
}

pub fn inc_verify_ok() {
    GLOBAL.verify_ok.fetch_add(1, Ordering::Relaxed);
}
pub fn inc_verify_fail() {
    GLOBAL.verify_fail.fetch_add(1, Ordering::Relaxed);
}
pub fn inc_replay_reject() {
    GLOBAL.replay_reject.fetch_add(1, Ordering::Relaxed);
}
pub fn inc_exec_ok() {
    GLOBAL.exec_verify_ok.fetch_add(1, Ordering::Relaxed);
}
pub fn inc_exec_fail() {
    GLOBAL.exec_verify_fail.fetch_add(1, Ordering::Relaxed);
}
pub fn inc_role_deny() {
    GLOBAL.role_deny.fetch_add(1, Ordering::Relaxed);
}
