// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Node health checks (spur#801).
//!
//! A node that finishes a job becomes immediately eligible for the next one,
//! with nothing verifying the machine is still in the state the controller
//! believes it is in — a partition-mode change, a GPU that fell off the bus, a
//! leaked allocation, an `amdgpu` reset in `dmesg`. The first job to land on a
//! degraded node then absorbs the failure (or, worse, a wrong result), and the
//! failure is attributed to the job, not the machine.
//!
//! When `[health] program` is set, spurd runs it as an operator-authored probe:
//!
//! * **before re-entry** — after a job completes, in the monitor loop, so a
//!   failing node is drained in the same completion report (see `agent_server`);
//! * **on an interval** — the periodic task here, so a node degrading while idle
//!   is pulled without waiting for the next job.
//!
//! A non-zero exit or a timeout is a failure; the program's output becomes the
//! drain reason. The check only ever *drains* — it never auto-resumes a node, so
//! it cannot silently undo an operator's manual drain.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use spur_core::config::HealthConfig;
use tracing::{debug, warn};

use crate::reporter::NodeReporter;

/// The result of one health-check run.
pub enum HealthOutcome {
    /// The program exited 0 within the timeout.
    Healthy,
    /// Non-zero exit, a timeout, or a failure to launch; the string is the
    /// drain reason (the program's output, or why it could not run).
    Unhealthy(String),
}

/// Longest drain reason we forward to the controller; a runaway probe should not
/// push an unbounded string through the RPC.
const MAX_REASON_LEN: usize = 800;

/// Most stdout+stderr we buffer per stream from a health probe. The drain reason
/// is short anyway, so cap capture to keep a chatty (or runaway) probe from
/// driving spurd's memory up.
const MAX_CAPTURE_BYTES: u64 = 64 * 1024;

/// Run the health program once, killing it (and failing) if it exceeds
/// `timeout_secs`. `kill_on_drop` ensures a timed-out probe leaves no orphan.
pub async fn run_once(program: &str, timeout_secs: u64) -> HealthOutcome {
    let mut child = match tokio::process::Command::new(program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return HealthOutcome::Unhealthy(format!(
                "health check '{program}' failed to start: {e}"
            ));
        }
    };

    // max(1): a configured 0 would make the check fail instantly, which is never
    // what an operator means by "no timeout". Report the effective value so the
    // message matches what actually happened.
    let effective = timeout_secs.max(1);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Read both streams concurrently with the wait so a full pipe can't deadlock
    // the child; each is capped at MAX_CAPTURE_BYTES.
    let capture = async move {
        let (out, err, status) =
            tokio::join!(read_capped(stdout), read_capped(stderr), child.wait(),);
        (status, out, err)
    };

    match tokio::time::timeout(Duration::from_secs(effective), capture).await {
        Err(_) => HealthOutcome::Unhealthy(format!(
            "health check '{program}' timed out after {effective}s"
        )),
        Ok((Err(e), _, _)) => {
            HealthOutcome::Unhealthy(format!("health check '{program}' errored: {e}"))
        }
        Ok((Ok(status), _, _)) if status.success() => HealthOutcome::Healthy,
        Ok((Ok(status), out, err)) => {
            HealthOutcome::Unhealthy(summarize_failure(program, status.code(), &out, &err))
        }
    }
}

/// Read up to `MAX_CAPTURE_BYTES` from an optional child stream.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    if let Some(s) = stream {
        let _ = s.take(MAX_CAPTURE_BYTES).read_to_end(&mut buf).await;
    }
    buf
}

/// Build a drain reason from a failed run: the exit status plus the program's
/// output (stderr preferred, then stdout), trimmed and length-bounded.
fn summarize_failure(
    program: &str,
    code: Option<i32>,
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
) -> String {
    let status = code
        .map(|c| format!("exit {c}"))
        .unwrap_or_else(|| "killed by signal".into());

    let stderr = String::from_utf8_lossy(stderr_bytes);
    let stdout = String::from_utf8_lossy(stdout_bytes);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };

    let mut reason = if detail.is_empty() {
        format!("health check '{program}' failed ({status})")
    } else {
        format!("health check '{program}' failed ({status}): {detail}")
    };
    if reason.len() > MAX_REASON_LEN {
        // Truncate on a char boundary so the String stays valid UTF-8.
        let mut end = MAX_REASON_LEN;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
        reason.push('…');
    }
    reason
}

/// Periodic health-check loop: run the program every `interval_secs` and drain
/// the node on failure. This complements the per-completion gate in the monitor
/// loop by catching a node that degrades while it is idle. Returns immediately
/// (does nothing) when no program is configured or the interval is 0.
pub async fn periodic_loop(cfg: HealthConfig, reporter: Arc<NodeReporter>) {
    let program = match cfg.program.clone() {
        Some(p) => p,
        None => return,
    };
    if cfg.interval_secs == 0 {
        debug!("health check interval is 0; periodic checking disabled (re-entry gate still runs)");
        return;
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.interval_secs));
    // The first tick fires immediately, giving a check at startup.
    loop {
        ticker.tick().await;
        if let HealthOutcome::Unhealthy(reason) = run_once(&program, cfg.timeout_secs).await {
            warn!(%reason, "periodic node health check failed — draining node");
            reporter.drain_node(&reason).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn zero_exit_is_healthy() {
        assert!(matches!(run_once("true", 5).await, HealthOutcome::Healthy));
    }

    #[tokio::test]
    async fn nonzero_exit_is_unhealthy() {
        assert!(matches!(
            run_once("false", 5).await,
            HealthOutcome::Unhealthy(_)
        ));
    }

    #[tokio::test]
    async fn missing_program_is_unhealthy_not_a_panic() {
        match run_once("/nonexistent/spur-health-probe", 5).await {
            HealthOutcome::Unhealthy(r) => assert!(r.contains("failed to start")),
            HealthOutcome::Healthy => panic!("a missing program must not read as healthy"),
        }
    }

    #[tokio::test]
    async fn timeout_is_unhealthy() {
        // sleep 10 under a 1s timeout: must be reported unhealthy well before 10s.
        let start = std::time::Instant::now();
        let out = run_once_argv(&["sleep", "10"], 1).await;
        assert!(matches!(out, HealthOutcome::Unhealthy(_)));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout was not enforced"
        );
    }

    #[tokio::test]
    async fn failure_reason_carries_program_output() {
        let out = run_once_argv(&["sh", "-c", "echo boom >&2; exit 3"], 5).await;
        match out {
            HealthOutcome::Unhealthy(r) => {
                assert!(r.contains("boom"), "reason missing stderr: {r}");
                assert!(r.contains("exit 3"), "reason missing status: {r}");
            }
            HealthOutcome::Healthy => panic!("expected unhealthy"),
        }
    }

    /// Test helper: run an argv (so tests can pass args without a wrapper file).
    async fn run_once_argv(argv: &[&str], timeout_secs: u64) -> HealthOutcome {
        let child = tokio::process::Command::new(argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn test probe");
        let wait = tokio::time::timeout(
            Duration::from_secs(timeout_secs.max(1)),
            child.wait_with_output(),
        )
        .await;
        match wait {
            Err(_) => HealthOutcome::Unhealthy("timed out".into()),
            Ok(Err(e)) => HealthOutcome::Unhealthy(format!("errored: {e}")),
            Ok(Ok(output)) if output.status.success() => HealthOutcome::Healthy,
            Ok(Ok(output)) => HealthOutcome::Unhealthy(summarize_failure(
                argv[0],
                output.status.code(),
                &output.stdout,
                &output.stderr,
            )),
        }
    }
}
