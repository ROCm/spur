// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::warn;

use spur_core::job::{JobId, JobState};

pub struct JobStartRecord {
    pub job_id: JobId,
    pub name: String,
    pub user: String,
    pub account: String,
    pub partition: String,
    pub num_nodes: u32,
    pub num_tasks: u32,
    pub cpus_per_task: u32,
    pub memory_mb: u64,
    pub submit_time: DateTime<Utc>,
    pub start_time: DateTime<Utc>,
    pub reservation: Option<String>,
}

pub struct AccountingNotifier {
    pool: PgPool,
}

impl AccountingNotifier {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn notify_job_start(&self, record: JobStartRecord) {
        let pool = self.pool.clone();
        let job_id = record.job_id;
        let name = record.name;
        let user = record.user;
        let account = record.account;
        let partition = record.partition;
        let num_nodes = record.num_nodes as i32;
        let num_tasks = record.num_tasks as i32;
        let cpus_per_task = record.cpus_per_task as i32;
        let memory_mb = record.memory_mb as i64;
        let submit_time = record.submit_time;
        let start_time = record.start_time;
        let reservation = record.reservation.unwrap_or_default();
        tokio::spawn(async move {
            if let Err(e) = super::db::record_job_start(
                &pool,
                job_id as i32,
                &name,
                &user,
                &account,
                &partition,
                num_nodes,
                num_tasks,
                cpus_per_task,
                memory_mb,
                submit_time,
                start_time,
                &reservation,
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
