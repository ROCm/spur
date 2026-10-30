// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-node RDMA fabric health check, meant to be named as the `program` of a
//! `[[health.checks]]` entry. Exit 0 on pass or skip, 1 when a check fails.

mod checks;
mod sysfs;

use checks::{evaluate, has_failures, tx_probe, Finding, Options, Status};
use clap::Parser;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Required number of RDMA devices. Defaults to the GPU count.
    #[arg(long)]
    expected_nics: Option<usize>,

    /// Do not compare the RDMA device count with the GPU count.
    #[arg(long)]
    no_gpu_match: bool,

    /// Filesystem root holding `sys/`; for testing against a fixture tree.
    #[arg(long, default_value = "/", hide = true)]
    root: PathBuf,

    /// Optional tx probe: a command, given after `--`, run once per ACTIVE
    /// port with `{dev}` and `{port}` replaced. A port whose tx counters do not
    /// advance is reported as failed.
    #[arg(last = true)]
    tx_probe: Vec<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let inv = sysfs::collect(&args.root);
    let opts = Options {
        expected_nics: args.expected_nics,
        match_gpus: !args.no_gpu_match,
    };
    let mut findings = evaluate(&inv, &opts);
    if let (false, Some(devices)) = (args.tx_probe.is_empty(), &inv.rdma) {
        findings.push(tx_probe(&args.root, devices, |dev, port| {
            run_probe_command(&args.tx_probe, dev, port)
        }));
    }
    for f in &findings {
        print_finding(f);
    }
    if has_failures(&findings) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print_finding(f: &Finding) {
    let label = match f.status {
        Status::Pass => "PASS",
        Status::Fail => "FAIL",
        Status::Skip => "SKIP",
    };
    println!("{label} {}: {}", f.check, f.detail);
}

fn run_probe_command(argv: &[String], dev: &str, port: u32) -> std::io::Result<bool> {
    let port = port.to_string();
    let expand = |a: &String| a.replace("{dev}", dev).replace("{port}", &port);
    Command::new(expand(&argv[0]))
        .args(argv[1..].iter().map(expand))
        .status()
        .map(|s| s.success())
}

#[cfg(test)]
mod tests;
