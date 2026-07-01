// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::warn;

use spur_core::job::{JobId, JobState};
use spur_core::resource::ResourceAllocations;

pub struct AccountingNotifier {
    pool: PgPool,
}

impl AccountingNotifier {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn notify_job_start(
        &self,
        job_id: JobId,
        user: String,
        account: String,
        partition: String,
        resources: &ResourceAllocations,
        start_time: DateTime<Utc>,
    ) {
        let pool = self.pool.clone();
        let cpus = resources.cpus as i32;
        let memory_mb = resources.memory_mb as i64;
        tokio::spawn(async move {
            if let Err(e) = super::db::record_job_start(
                &pool,
                job_id as i32,
                &user,
                &account,
                &partition,
                1,
                cpus,
                1,
                memory_mb,
                start_time,
            )
            .await
            {
                warn!(job_id, error = %e, "failed to record job start in accounting");
            }
        });
    }

    pub fn notify_job_end(
        &self,
        job_id: JobId,
        state: JobState,
        exit_code: i32,
        end_time: DateTime<Utc>,
        exit_signal: i32,
        derived_exit_code: i32,
    ) {
        let pool = self.pool.clone();
        let state_str = state.display().to_owned();
        tokio::spawn(async move {
            if let Err(e) = super::db::record_job_end(
                &pool,
                job_id as i32,
                &state_str,
                exit_code,
                end_time,
                exit_signal,
                derived_exit_code,
            )
            .await
            {
                warn!(job_id, error = %e, "failed to record job end in accounting");
            }
        });
    }
}
