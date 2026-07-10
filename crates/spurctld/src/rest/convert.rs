// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use spur_core::job::{Job, JobState};
use spur_core::node::Node;
use spur_core::partition::Partition;

pub fn job_to_json(job: &Job) -> serde_json::Value {
    serde_json::json!({
        "job_id": job.job_id,
        "name": job.spec.name,
        "user_name": job.spec.user,
        "user_id": job.spec.uid,
        "partition": job.spec.partition,
        "account": job.spec.account,
        "job_state": job.state.display(),
        "state_reason": job.pending_reason.display(),
        "submit_time": job.submit_time.timestamp(),
        "start_time": job.start_time.map(|t| t.timestamp()),
        "end_time": job.end_time.map(|t| t.timestamp()),
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

pub fn node_to_json(node: &Node) -> serde_json::Value {
    serde_json::json!({
        "name": node.name,
        "state": node.state.display(),
        "reason": node.state_reason,
        "partitions": node.partitions,
        "cpus": node.total_resources.cpus,
        "alloc_cpus": node.alloc_resources.cpus,
        "real_memory": node.total_resources.memory_mb,
        "free_mem": node.free_memory_mb,
        "cpu_load": node.cpu_load,
        "architecture": node.arch,
        "operating_system": node.os,
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
