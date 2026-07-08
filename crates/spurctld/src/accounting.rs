// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use tonic::transport::Channel;
use tracing::warn;

use spur_core::job::{JobId, JobState};
use spur_core::resource::ResourceAllocations;
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::{RecordJobEndRequest, RecordJobStartRequest};

use crate::server::{allocations_to_proto, datetime_to_proto};

pub struct JobStartRecord {
    pub job_id: JobId,
    pub user: String,
    pub account: String,
    pub partition: String,
    pub resources: ResourceAllocations,
    pub start_time: DateTime<Utc>,
    pub reservation: Option<String>,
}

pub struct AccountingNotifier {
    client: SlurmAccountingClient<Channel>,
}

impl AccountingNotifier {
    pub async fn connect(host: &str) -> anyhow::Result<Self> {
        let uri = if host.starts_with("http://") || host.starts_with("https://") {
            host.to_string()
        } else {
            format!("http://{}", host)
        };
        let client = SlurmAccountingClient::connect(uri).await?;
        Ok(Self { client })
    }

    pub fn notify_job_start(&self, record: JobStartRecord) {
        let req = RecordJobStartRequest {
            job_id: record.job_id,
            user: record.user,
            account: record.account,
            partition: record.partition,
            resources: Some(allocations_to_proto(&record.resources)),
            start_time: Some(datetime_to_proto(record.start_time)),
            reservation: record.reservation.unwrap_or_default(),
        };
        let mut client = self.client.clone();
        let job_id = record.job_id;
        tokio::spawn(async move {
            if let Err(e) = client.record_job_start(req).await {
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
        let req = RecordJobEndRequest {
            job_id,
            final_state: state.to_proto_i32(),
            exit_code,
            end_time: Some(datetime_to_proto(end_time)),
            exit_signal,
            derived_exit_code,
        };
        let mut client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = client.record_job_end(req).await {
                warn!(job_id, error = %e, "failed to record job end in accounting");
            }
        });
    }
}
