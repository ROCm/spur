// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use tonic::transport::Channel;
use tracing::warn;

use spur_core::job::{JobId, JobState};
use spur_core::resource::ResourceSet;
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::{RecordJobEndRequest, RecordJobStartRequest};

use crate::server::{datetime_to_proto, job_state_to_proto, resource_to_proto};

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

    pub fn notify_job_start(
        &self,
        job_id: JobId,
        user: String,
        account: String,
        partition: String,
        resources: &ResourceSet,
        start_time: DateTime<Utc>,
    ) {
        let req = RecordJobStartRequest {
            job_id,
            user,
            account,
            partition,
            resources: Some(resource_to_proto(resources)),
            start_time: Some(datetime_to_proto(start_time)),
        };
        let mut client = self.client.clone();
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
    ) {
        let req = RecordJobEndRequest {
            job_id,
            final_state: job_state_to_proto(state) as i32,
            exit_code,
            end_time: Some(datetime_to_proto(end_time)),
        };
        let mut client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = client.record_job_end(req).await {
                warn!(job_id, error = %e, "failed to record job end in accounting");
            }
        });
    }
}
