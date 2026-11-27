// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::timefmt::format_duration_dhms as format_duration;
use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::GetJobRequest;

/// Display status information for running jobs.
#[derive(Parser, Debug)]
#[command(name = "sstat", about = "Display status of running jobs")]
pub struct SstatArgs {
    /// Job ID to query
    #[arg(short = 'j', long = "jobs", required = true)]
    pub job_id: String,

    /// Output format (comma-separated field names)
    #[arg(short = 'o', long)]
    pub format: Option<String>,

    /// Don't print header
    #[arg(long)]
    pub noheader: bool,

    /// Accepted for Slurm compatibility; has no effect
    #[arg(long)]
    pub noconvert: bool,

    /// Parsable output (delimiter-separated)
    #[arg(short = 'p', long)]
    pub parsable: bool,

    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    pub controller: String,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = crate::clap_exit::parse_or_exit::<SstatArgs>(&args);

    let job_ids = crate::job_id_arg::parse_job_ids(&args.job_id)?;

    let channel = crate::authclient::connect(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    // Determine which fields to show
    let fields = if let Some(ref fmt) = args.format {
        parse_field_list(fmt)
    } else {
        default_fields()
    };

    let delimiter = if args.parsable { "|" } else { "  " };

    // Print header
    if !args.noheader {
        let headers: Vec<String> = fields.iter().map(format_header).collect();
        if args.parsable {
            println!("{}|", headers.join(delimiter));
        } else {
            println!("{}", headers.join(delimiter));
        }
        if !args.parsable {
            let sep: Vec<String> = fields.iter().map(|f| "-".repeat(field_width(f))).collect();
            println!("{}", sep.join(delimiter));
        }
    }

    for job_id in &job_ids {
        let response = client
            .get_job(GetJobRequest { job_id: *job_id })
            .await
            .context(format!("failed to get job {}", job_id))?;

        let job = response.into_inner();

        // Only show running jobs
        if job.state != spur_proto::proto::JobState::JobRunning as i32 {
            eprintln!(
                "sstat: job {} is not running (state: {})",
                job_id,
                state_name(job.state)
            );
            continue;
        }

        let values: Vec<String> = fields.iter().map(|f| resolve_field(&job, f)).collect();
        if args.parsable {
            println!("{}|", values.join(delimiter));
        } else {
            // Pad each value to field width
            let padded: Vec<String> = fields
                .iter()
                .zip(values.iter())
                .map(|(f, v)| format!("{:>width$}", v, width = field_width(f)))
                .collect();
            println!("{}", padded.join(delimiter));
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
enum StatField {
    JobId,
    AveCpu,
    AveRss,
    AveVmSize,
    MaxRss,
    MaxVmSize,
    NTasks,
    NodeList,
    Cpus,
    MemAlloc,
    GpuAlloc,
    State,
    Elapsed,
}

fn default_fields() -> Vec<StatField> {
    vec![
        StatField::JobId,
        StatField::NTasks,
        StatField::Cpus,
        StatField::MemAlloc,
        StatField::GpuAlloc,
        StatField::Elapsed,
        StatField::NodeList,
    ]
}

fn parse_field_list(fmt: &str) -> Vec<StatField> {
    fmt.split(',')
        .filter_map(|name| match name.trim().to_lowercase().as_str() {
            "jobid" => Some(StatField::JobId),
            "avecpu" => Some(StatField::AveCpu),
            "averss" => Some(StatField::AveRss),
            "avevmsize" => Some(StatField::AveVmSize),
            "maxrss" => Some(StatField::MaxRss),
            "maxvmsize" => Some(StatField::MaxVmSize),
            "ntasks" => Some(StatField::NTasks),
            "nodelist" => Some(StatField::NodeList),
            "cpus" | "ncpus" => Some(StatField::Cpus),
            "memalloc" | "reqmem" => Some(StatField::MemAlloc),
            "gpualloc" | "gres" => Some(StatField::GpuAlloc),
            "state" => Some(StatField::State),
            "elapsed" => Some(StatField::Elapsed),
            _ => {
                eprintln!("sstat: unknown field '{}'", name.trim());
                None
            }
        })
        .collect()
}

fn format_header(field: &StatField) -> String {
    let (name, width) = header_info(field);
    format!("{:>width$}", name, width = width)
}

fn field_width(field: &StatField) -> usize {
    header_info(field).1
}

fn header_info(field: &StatField) -> (&'static str, usize) {
    match field {
        StatField::JobId => ("JobID", 10),
        StatField::AveCpu => ("AveCPU", 10),
        StatField::AveRss => ("AveRSS", 10),
        StatField::AveVmSize => ("AveVMSize", 10),
        StatField::MaxRss => ("MaxRSS", 10),
        StatField::MaxVmSize => ("MaxVMSize", 10),
        StatField::NTasks => ("NTasks", 8),
        StatField::NodeList => ("Nodelist", 20),
        StatField::Cpus => ("NCPUS", 8),
        StatField::MemAlloc => ("MemAlloc", 10),
        StatField::GpuAlloc => ("GPUAlloc", 10),
        StatField::State => ("State", 10),
        StatField::Elapsed => ("Elapsed", 12),
    }
}

fn resolve_field(job: &spur_proto::proto::JobInfo, field: &StatField) -> String {
    match field {
        StatField::JobId => job.job_id.to_string(),
        StatField::NTasks => job.num_tasks.to_string(),
        StatField::Cpus => crate::sacct::ncpus(job).to_string(),
        StatField::MemAlloc => {
            if let Some(ref res) = job.resources {
                format!("{}M", res.memory_mb)
            } else {
                "0M".into()
            }
        }
        StatField::GpuAlloc => {
            if let Some(ref res) = job.resources {
                res.devices
                    .get("gpu")
                    .map(|d| d.devices.iter().map(|dev| dev.count).sum::<u64>())
                    .unwrap_or(0)
                    .to_string()
            } else {
                "0".into()
            }
        }
        StatField::NodeList => job.nodelist.clone(),
        StatField::State => state_name(job.state).to_string(),
        StatField::Elapsed => {
            if let Some(ref rt) = job.run_time {
                format_duration(rt.seconds)
            } else {
                "00:00:00".into()
            }
        }
        // These fields would require real-time process stats from the agent.
        // For now, show N/A since we don't poll agents for per-process metrics.
        StatField::AveCpu => "N/A".into(),
        StatField::AveRss => "N/A".into(),
        StatField::AveVmSize => "N/A".into(),
        StatField::MaxRss => "N/A".into(),
        StatField::MaxVmSize => "N/A".into(),
    }
}

fn state_name(state: i32) -> &'static str {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.display())
        .unwrap_or("UNKNOWN")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bad_job_id_is_rejected_before_connect() {
        let err = main_with_args(vec![
            "sstat".into(),
            "-j".into(),
            "30,abc".into(),
            "--controller".into(),
            "http://127.0.0.1:1".into(),
        ])
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Invalid job id specified: abc"), "{err}");
    }

    #[test]
    fn noconvert_is_accepted() {
        // Slurm scripts pass --noconvert to keep output machine-parseable.
        let args = SstatArgs::try_parse_from(["sstat", "--noconvert", "--jobs", "1"]).unwrap();
        assert!(args.noconvert);
        assert_eq!(args.job_id, "1");
    }

    #[test]
    fn parse_field_list_accepts_aliases_case_and_padding() {
        assert_eq!(parse_field_list("jobid"), [StatField::JobId]);
        assert_eq!(parse_field_list("NCPUS"), [StatField::Cpus]);
        assert_eq!(parse_field_list("cpus"), [StatField::Cpus]);
        assert_eq!(parse_field_list("reqmem"), [StatField::MemAlloc]);
        assert_eq!(parse_field_list("gres"), [StatField::GpuAlloc]);
        assert_eq!(
            parse_field_list("  JobID , Elapsed  "),
            [StatField::JobId, StatField::Elapsed]
        );
    }

    #[test]
    fn parse_field_list_skips_unknown_fields_but_keeps_the_rest() {
        assert_eq!(
            parse_field_list("jobid,bogus,elapsed"),
            [StatField::JobId, StatField::Elapsed]
        );
        assert!(parse_field_list("bogus").is_empty());
    }

    #[test]
    fn header_width_matches_formatted_header() {
        for field in default_fields() {
            assert_eq!(format_header(&field).len(), field_width(&field));
        }
    }

    #[test]
    fn state_name_falls_back_to_unknown() {
        assert_eq!(state_name(i32::MAX), "UNKNOWN");
    }

    #[test]
    fn cpus_field_uses_the_req_tres_cpu_value_when_present() {
        // req_tres already counts --cpus-per-gpu, so it must win over the naive
        // num_tasks * cpus_per_task product.
        let job = spur_proto::proto::JobInfo {
            req_tres: "cpu=16,node=1".into(),
            num_tasks: 1,
            cpus_per_task: 1,
            ..Default::default()
        };
        assert_eq!(resolve_field(&job, &StatField::Cpus), "16");
    }

    #[test]
    fn cpus_field_falls_back_when_req_tres_is_empty() {
        let job = spur_proto::proto::JobInfo {
            num_tasks: 4,
            cpus_per_task: 2,
            ..Default::default()
        };
        assert_eq!(resolve_field(&job, &StatField::Cpus), "8");
    }

    /// Two nodes' GPUs at the same PCI address share a stable id, so the
    /// job-wide total merges them into one entry whose count is 2.
    #[test]
    fn gpu_alloc_counts_merged_devices_of_a_multi_node_job() {
        use spur_proto::proto::{AllocatedDevice, DeviceAllocations, ResourceAllocations};
        let job = spur_proto::proto::JobInfo {
            resources: Some(ResourceAllocations {
                devices: [(
                    "gpu".to_string(),
                    DeviceAllocations {
                        devices: vec![
                            AllocatedDevice {
                                device_id: 0x110000,
                                count: 2,
                            },
                            AllocatedDevice {
                                device_id: 0x120000,
                                count: 2,
                            },
                        ],
                    },
                )]
                .into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_field(&job, &StatField::GpuAlloc), "4");
    }
}
