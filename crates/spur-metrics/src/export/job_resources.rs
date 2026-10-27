// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-job CPU, memory, and disk series for `/metrics/job-resources`.
//!
//! Opt-in (`metrics.high_cardinality`): `job_id` is a new series for every job.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use spur_core::job_usage::JobUsage;
use std::sync::atomic::AtomicU64;

use crate::export::encode_registered;

/// One node's reading for one job, with the job's labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledJobUsage {
    pub node: String,
    pub user: String,
    pub partition: String,
    pub account: String,
    pub usage: JobUsage,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct JobResourceLabel {
    node: String,
    job_id: String,
    job_user: String,
    job_partition: String,
    job_account: String,
}

fn label_of(sample: &LabeledJobUsage) -> JobResourceLabel {
    JobResourceLabel {
        node: sample.node.clone(),
        job_id: sample.usage.job_id.to_string(),
        job_user: sample.user.clone(),
        job_partition: sample.partition.clone(),
        job_account: sample.account.clone(),
    }
}

/// Register per-job resource series. Empty input registers nothing.
pub fn register_job_resources(registry: &mut Registry, samples: &[LabeledJobUsage]) {
    if samples.is_empty() {
        return;
    }

    let cpu = Family::<JobResourceLabel, Counter<f64, AtomicU64>>::default();
    let rss = Family::<JobResourceLabel, Gauge<u64, AtomicU64>>::default();
    let virtual_mem = Family::<JobResourceLabel, Gauge<u64, AtomicU64>>::default();
    let swap = Family::<JobResourceLabel, Gauge<u64, AtomicU64>>::default();
    let disk_read = Family::<JobResourceLabel, Counter<u64, AtomicU64>>::default();
    let disk_write = Family::<JobResourceLabel, Counter<u64, AtomicU64>>::default();

    for sample in samples {
        let label = label_of(sample);
        let usage = &sample.usage;
        cpu.get_or_create(&label)
            .inc_by(usage.cpu_usage_usec as f64 / 1e6);
        rss.get_or_create(&label).set(usage.rss_bytes);
        virtual_mem.get_or_create(&label).set(usage.virtual_bytes);
        swap.get_or_create(&label).set(usage.swap_bytes);
        disk_read
            .get_or_create(&label)
            .inc_by(usage.disk_read_bytes);
        disk_write
            .get_or_create(&label)
            .inc_by(usage.disk_write_bytes);
    }

    registry.register(
        "spur_job_cpu_seconds",
        "Cumulative CPU time of the job on this node, in seconds",
        cpu,
    );
    registry.register(
        "spur_job_memory_rss_bytes",
        "Resident set size of the job's processes on this node, in bytes",
        rss,
    );
    registry.register(
        "spur_job_memory_virtual_bytes",
        "Virtual memory size of the job's processes on this node, in bytes",
        virtual_mem,
    );
    registry.register(
        "spur_job_memory_swap_bytes",
        "Swap charged to the job cgroup on this node, in bytes",
        swap,
    );
    registry.register(
        "spur_job_disk_read_bytes",
        "Bytes read from storage by the job's processes on this node",
        disk_read,
    );
    registry.register(
        "spur_job_disk_write_bytes",
        "Bytes written to storage by the job's processes on this node",
        disk_write,
    );
}

/// Encode per-job resource metrics as OpenMetrics 1.0 text.
pub fn encode_job_resource_metrics(samples: &[LabeledJobUsage]) -> String {
    encode_registered(|registry| register_job_resources(registry, samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        job_id: u32,
        user: &str,
        partition: &str,
        account: &str,
        values: [u64; 6],
    ) -> LabeledJobUsage {
        let [cpu_usage_usec, rss_bytes, virtual_bytes, swap_bytes, disk_read_bytes, disk_write_bytes] =
            values;
        LabeledJobUsage {
            node: "n1".into(),
            user: user.into(),
            partition: partition.into(),
            account: account.into(),
            usage: JobUsage {
                job_id,
                cpu_usage_usec,
                rss_bytes,
                virtual_bytes,
                swap_bytes,
                disk_read_bytes,
                disk_write_bytes,
            },
        }
    }

    fn line<'a>(body: &'a str, metric: &str, job_id: u32) -> &'a str {
        let marker = format!("job_id=\"{job_id}\"");
        body.lines()
            .find(|line| line.starts_with(metric) && line.contains(&marker))
            .unwrap_or("")
    }

    #[test]
    fn empty_input_is_only_the_eof_marker() {
        assert_eq!(encode_job_resource_metrics(&[]), "# EOF\n");
    }

    #[test]
    fn every_resource_carries_the_same_labels() {
        let body = encode_job_resource_metrics(&[sample(
            7,
            "alice",
            "debug",
            "team",
            [1_500_000, 22, 33, 44, 55, 66],
        )]);
        for metric in [
            "spur_job_cpu_seconds_total",
            "spur_job_memory_rss_bytes",
            "spur_job_memory_virtual_bytes",
            "spur_job_memory_swap_bytes",
            "spur_job_disk_read_bytes_total",
            "spur_job_disk_write_bytes_total",
        ] {
            let row = line(&body, metric, 7);
            assert!(row.contains("node=\"n1\""), "{metric}: {row}");
            assert!(row.contains("job_id=\"7\""), "{metric}: {row}");
            assert!(row.contains("job_user=\"alice\""), "{metric}: {row}");
            assert!(row.contains("job_partition=\"debug\""), "{metric}: {row}");
            assert!(row.contains("job_account=\"team\""), "{metric}: {row}");
        }
        assert!(line(&body, "spur_job_cpu_seconds_total", 7).contains("} 1.5"));
        assert!(line(&body, "spur_job_memory_rss_bytes", 7).contains("} 22"));
        assert!(line(&body, "spur_job_memory_virtual_bytes", 7).contains("} 33"));
        assert!(line(&body, "spur_job_memory_swap_bytes", 7).contains("} 44"));
        assert!(line(&body, "spur_job_disk_read_bytes_total", 7).contains("} 55"));
        assert!(line(&body, "spur_job_disk_write_bytes_total", 7).contains("} 66"));
        assert!(!body.contains("spur_node_cpu_load"));
        assert!(!body.contains("spur_node_free_memory_bytes"));
    }

    #[test]
    fn two_jobs_on_one_node_stay_on_their_own_series() {
        let body = encode_job_resource_metrics(&[
            sample(7, "alice", "debug", "team", [11, 22, 0, 0, 0, 0]),
            sample(9, "bob", "debug", "other", [99, 100, 0, 0, 0, 0]),
        ]);
        let alice = line(&body, "spur_job_memory_rss_bytes", 7);
        let bob = line(&body, "spur_job_memory_rss_bytes", 9);
        assert!(alice.contains("job_user=\"alice\""));
        assert!(alice.contains("job_account=\"team\""));
        assert!(alice.contains("} 22"));
        assert!(!alice.contains("bob"));
        assert!(bob.contains("job_user=\"bob\""));
        assert!(bob.contains("job_account=\"other\""));
        assert!(bob.contains("} 100"));
        assert!(!bob.contains("alice"));
    }

    #[test]
    fn a_job_with_no_account_still_has_the_label() {
        let body = encode_job_resource_metrics(&[sample(1, "alice", "", "", [0, 0, 0, 0, 0, 0])]);
        let row = line(&body, "spur_job_memory_rss_bytes", 1);
        assert!(row.contains("job_account=\"\""));
        assert!(row.contains("job_partition=\"\""));
        assert!(row.contains("} 0"));
    }
}
