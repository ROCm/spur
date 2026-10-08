// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Counters for the best-effort `txn` write. A dropped row means an action
//! happened that the log cannot account for, which must be alertable.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct AuditMetrics {
    pub rows_written: AtomicU64,
    pub rows_dropped: AtomicU64,
}

impl AuditMetrics {
    pub fn snapshot(&self) -> AuditMetricsSnapshot {
        AuditMetricsSnapshot {
            rows_written: self.rows_written.load(Ordering::Relaxed),
            rows_dropped: self.rows_dropped.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditMetricsSnapshot {
    pub rows_written: u64,
    pub rows_dropped: u64,
}

static GLOBAL: AuditMetrics = AuditMetrics {
    rows_written: AtomicU64::new(0),
    rows_dropped: AtomicU64::new(0),
};

pub fn global() -> &'static AuditMetrics {
    &GLOBAL
}

pub fn inc_rows_written() {
    GLOBAL.rows_written.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_rows_dropped() {
    GLOBAL.rows_dropped.fetch_add(1, Ordering::Relaxed);
}
