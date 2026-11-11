// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spurstepd`: the per-job supervisor spurd hands a job off to, running for
//! the job's whole lifetime independent of spurd's own restarts.

fn main() -> anyhow::Result<()> {
    // Checked before any runtime exists: the task launcher forks after setns
    // and privilege changes, which is only safe in a single-threaded process.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == spurd::task_launcher::SUBCOMMAND)
    {
        let args: Vec<String> = std::env::args().skip(2).collect();
        spurd::task_launcher::run_from_args(&args);
    }

    if std::env::args_os()
        .skip(1)
        .any(|a| a == "-V" || a == "--version")
    {
        println!("{}", spur_core::version::version_string());
        return Ok(());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(supervise())
}

async fn supervise() -> anyhow::Result<()> {
    // stderr is inherited across the double-fork, so these land in the agent log
    // instead of being discarded for the job's whole lifetime.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(version = %spur_core::version::version_string(), "spurstepd starting");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let exit_code = spurd::stepd::run_process(&args).await.map_err(|error| {
        eprintln!("spurstepd failed: {error:#}");
        error
    })?;
    std::process::exit(exit_code);
}
