// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use spur_core::job::{Job, JobState};
use spur_core::node::{Node, NodeState};
use spur_core::partition::Partition;

pub fn job_to_json(job: &Job) -> serde_json::Value {
    serde_json::json!({
        "job_id": job.job_id,
        "preemption_usage": {
            "used": job.preemption_usage.used,
            "unused": job.preemption_usage.unused_total(),
            "open": job.preemption_usage.open,
            "unused_by_cause": crate::metrics_proto::preemption_usage_to_proto(&job.preemption_usage).unused_by_cause.iter().map(|cause| serde_json::json!({
                "reason": cause.reason, "initiator": cause.initiator, "count": cause.count,
            })).collect::<Vec<_>>(),
        },
        "idle_fill": job.idle_fill,
        "name": job.spec.name,
        "user_name": job.spec.user,
        "user_id": job.spec.uid,
        "partition": job.spec.partition,
        "effective_partition": job.effective_partition,
        "account": job.spec.account,
        "job_state": job.state.display(),
        "state_reason": job.state_reason(),
        "submit_time": job.submit_time.timestamp(),
        "start_time": job.start_time.map(|t| t.timestamp()),
        "end_time": job.reported_end_time().map(|t| t.timestamp()),
        "preempt_time": job.preempt_grace().map(|p| p.signaled_at.timestamp()),
        "time_limit": job.spec.time_limit.map(|d| d.num_minutes()),
        "node_count": job.spec.num_nodes,
        "tasks": job.spec.num_tasks,
        "cpus_per_task": job.spec.cpus_per_task,
        "nodes": if job.allocated_nodes.is_empty() {
            String::new()
        } else {
            spur_core::hostlist::compress(&job.allocated_nodes)
        },
        "current_working_directory": job.spec.work_dir,
        "command": job.spec.script.as_deref()
            .and_then(|s| s.lines()
                .find(|l| !l.starts_with('#') && !l.trim().is_empty()))
            .unwrap_or(""),
        "exit_code": job.exit_code.unwrap_or(0),
        "standard_output": job.resolved_stdout(),
        "standard_error": job.resolved_stderr(),
        "priority": job.priority,
        "qos": job.spec.qos,
    })
}

pub fn node_to_json(
    node: &Node,
    planned: Option<(spur_core::job::JobId, chrono::DateTime<chrono::Utc>)>,
) -> serde_json::Value {
    let layout_drain = node.layout_drain_reason();
    let (state, reason, reason_uid, reason_time) = match &layout_drain {
        Some(reason) => (NodeState::Drain.display(), Some(reason.clone()), None, None),
        None => (
            node.state.display(),
            node.state_reason.clone(),
            node.reason_uid,
            node.reason_time.map(|t| t.to_rfc3339()),
        ),
    };
    serde_json::json!({
        "name": node.name,
        "state": state,
        "not_responding": node.not_responding,
        "reason": reason,
        "reason_uid": reason_uid,
        "reason_time": reason_time,
        "partitions": node.partitions,
        "cpus": node.cpu_tot(),
        "effective_cpus": node.total_resources.cpus,
        "alloc_cpus": node.alloc_resources.cpus,
        "real_memory": node.total_resources.memory_mb,
        "free_mem": node.free_memory_mb,
        "cpu_load": node.cpu_load,
        "architecture": node.arch,
        "operating_system": node.os,
        "planned_job_id": planned.map(|(id, _)| id),
        "planned_start": planned.map(|(_, start)| start.timestamp()),
    })
}

pub fn partition_to_json(part: &Partition) -> serde_json::Value {
    serde_json::json!({
        "name": part.name,
        "state": part.state.display(),
        "is_default": part.is_default,
        "total_nodes": 0,
        "total_cpus": 0,
        "nodes": part.nodes,
        "max_time": part.max_time_minutes,
        "default_time": part.default_time_minutes,
        "priority_tier": part.priority_tier,
        "allow_accounts": part.allow_accounts,
        "deny_accounts": part.deny_accounts,
        "max_cpus_per_node": part.max_tres_per_node.get(spur_core::partition::TRES_CPU),
        "max_tres_per_node": part.max_tres_per_node,
        "max_mem_per_node": part.max_mem_per_node,
        "max_mem_per_cpu": part.max_mem_per_cpu,
        "def_mem_per_node": part.def_mem_per_node,
        "def_mem_per_cpu": part.def_mem_per_cpu,
        "def_mem_per_gpu": part.def_mem_per_gpu,
    })
}

pub fn parse_states_query(s: &str) -> Result<Vec<JobState>, String> {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(Vec::new());
    }

    let tokens: Vec<&str> = trimmed
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return Err("Invalid job state specified: (empty)".into());
    }

    let mut states = Vec::with_capacity(tokens.len());
    for token in tokens {
        let core = JobState::from_code_or_name(token)
            .ok_or_else(|| format!("Invalid job state specified: {token}"))?;
        states.push(core);
    }
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_to_json_reports_preemption_counts_and_causes() {
        let mut job = Job::new(7, Default::default());
        job.preemption_usage.open_one();
        job.preemption_usage.settle(Some((
            spur_core::preemption_usage::UnusedReason::Cancelled,
            spur_core::preemption_usage::PreemptionInitiator::User,
        )));
        assert_eq!(
            job_to_json(&job)["preemption_usage"],
            serde_json::json!({
                "used": 0, "unused": 1, "open": 0,
                "unused_by_cause": [{"reason": "cancelled", "initiator": "user", "count": 1}],
            })
        );
    }

    #[test]
    fn job_to_json_reports_grace_preempt_time_and_end_time() {
        use chrono::{Duration, TimeZone, Utc};
        use spur_core::job::{JobSpec, PreemptPending};
        use spur_core::partition::PreemptMode;

        let mut job = Job::new(7, JobSpec::default());
        let plain = job_to_json(&job);
        assert!(plain["preempt_time"].is_null());
        assert!(plain["end_time"].is_null());

        let signaled_at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let deadline = signaled_at + Duration::seconds(60);
        job.preempt_pending = Some(PreemptPending {
            signaled_at,
            deadline,
            preempted_by: 9,
            preempt_qos: None,
            mode: PreemptMode::Cancel,
            run_attempt: 0,
            requeue_hold_secs: 0,
        });
        let json = job_to_json(&job);
        assert_eq!(json["preempt_time"], signaled_at.timestamp());
        assert_eq!(json["end_time"], deadline.timestamp());

        let ended = signaled_at + Duration::seconds(10);
        job.end_time = Some(ended);
        assert_eq!(job_to_json(&job)["end_time"], ended.timestamp());

        job.run_attempt = 1;
        job.end_time = None;
        let next_run = job_to_json(&job);
        assert!(
            next_run["preempt_time"].is_null(),
            "a grace from an earlier run is not reported"
        );
        assert!(next_run["end_time"].is_null());
    }

    #[test]
    fn node_to_json_projects_a_core_layout_block_like_scontrol() {
        let mut node = Node::new("n1".into(), Default::default());
        node.state = NodeState::Idle;
        node.layout_enforce_error = Some("root write failed".into());

        let json = node_to_json(&node, None);
        assert_eq!(json["state"], NodeState::Drain.display());
        assert_eq!(
            json["reason"],
            "core-layout: not enforced: root write failed"
        );
    }

    #[test]
    fn partition_to_json_includes_access_control_fields() {
        let part = Partition {
            name: "gpu".into(),
            allow_accounts: vec!["research".into()],
            deny_accounts: vec!["student".into()],
            ..Default::default()
        };
        let json = partition_to_json(&part);
        assert_eq!(json["allow_accounts"], serde_json::json!(["research"]));
        assert_eq!(json["deny_accounts"], serde_json::json!(["student"]));
    }

    #[test]
    fn job_to_json_reports_the_effective_partition() {
        let mut job = Job::new(
            7,
            spur_core::job::JobSpec {
                partition: Some("cpu,wide".into()),
                ..Default::default()
            },
        );
        assert!(job_to_json(&job)["effective_partition"].is_null());
        job.effective_partition = Some("wide".into());
        let json = job_to_json(&job);
        assert_eq!(json["partition"], "cpu,wide");
        assert_eq!(json["effective_partition"], "wide");
    }

    #[test]
    fn partition_to_json_includes_resource_limits() {
        let part = Partition {
            name: "cpu".into(),
            max_tres_per_node: [("cpu".to_string(), 64), ("gres/gpu".to_string(), 0)].into(),
            max_mem_per_cpu: Some(2048),
            def_mem_per_node: Some(4096),
            ..Default::default()
        };
        let json = partition_to_json(&part);
        assert_eq!(json["max_cpus_per_node"], 64);
        assert_eq!(
            json["max_tres_per_node"],
            serde_json::json!({"cpu": 64, "gres/gpu": 0})
        );
        assert_eq!(json["max_mem_per_cpu"], 2048);
        assert_eq!(json["def_mem_per_node"], 4096);
        assert!(json["max_mem_per_node"].is_null());

        let plain = partition_to_json(&Partition::default());
        assert!(plain["max_cpus_per_node"].is_null());
        assert_eq!(plain["max_tres_per_node"], serde_json::json!({}));
    }

    #[test]
    fn parse_states_query_accepts_valid_tokens() {
        let states = parse_states_query("RUNNING,PD").unwrap();
        assert_eq!(states.len(), 2);
    }

    #[test]
    fn parse_states_query_rejects_unknown() {
        assert!(parse_states_query("BOGUS").is_err());
        assert!(parse_states_query("R,BOGUS").is_err());
    }

    #[test]
    fn parse_states_query_all_means_no_filter() {
        assert!(parse_states_query("all").unwrap().is_empty());
    }
}
