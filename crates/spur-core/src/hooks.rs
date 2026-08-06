// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::Stdio;

use anyhow::Context;
use chrono::{DateTime, Utc};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::job::JobSpec;
use crate::spur_env::SpurEnv;

pub type JobId = u32;

/// Context passed to prolog/epilog hook scripts via environment variables.
pub struct HookContext {
    pub job_id: JobId,
    pub work_dir: String,
    pub uid: u32,
    pub gid: u32,
    pub partition: String,
    pub nodelist: String,
    /// Identifies which hook is running. One of:
    /// `prolog_slurmd`, `epilog_slurmd`, `prolog_slurmctld`, `epilog_slurmctld`,
    /// `prolog_task`, `epilog_task`, `prolog_srun`, `epilog_srun`.
    pub script_context: String,
    pub gpu_devices: Vec<u32>,
    pub cpus: u32,
    pub memory_mb: u64,
}

/// Run a prolog/epilog hook script with rich environment variables.
///
/// Stderr is captured and logged; stdout is discarded.
/// Returns `Err` on script execution failure or non-zero exit.
pub async fn run_hook(script_path: &str, ctx: &HookContext) -> anyhow::Result<()> {
    info!(
        job_id = ctx.job_id,
        hook = %ctx.script_context,
        script = script_path,
        "running hook"
    );

    let username = resolve_username(ctx.uid);

    let gpu_list: String = ctx
        .gpu_devices
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let mut env = SpurEnv::new();
    env.set_with_slurm_twin("SPUR_JOB_ID", ctx.job_id);
    env.set_with_slurm_twin("SPUR_JOB_PARTITION", &ctx.partition);
    env.set_with_slurm_twin("SPUR_JOB_NODELIST", &ctx.nodelist);
    env.set_with_slurm_twin("SPUR_CPUS_ON_NODE", ctx.cpus);
    env.set("SPUR_JOB_USER", &username);
    env.set("SPUR_JOB_UID", ctx.uid);
    env.set("SPUR_JOB_GID", ctx.gid);
    env.set("SPUR_JOB_WORK_DIR", &ctx.work_dir);
    env.set("SPUR_JOB_GPUS", &gpu_list);
    env.set("SPUR_JOB_MEMORY_MB", ctx.memory_mb);
    env.set("SPUR_SCRIPT_CONTEXT", &ctx.script_context);

    let mut cmd = Command::new(script_path);
    for (k, v) in env.into_map() {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let child = spawn_hook_in_work_dir(&mut cmd, &ctx.work_dir, ctx.job_id, &ctx.script_context)
        .with_context(|| {
            format!(
                "{} script failed to execute: {}",
                ctx.script_context, script_path
            )
        })?;

    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("{} script failed to complete", ctx.script_context))?;

    if !output.stderr.is_empty() {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        for line in stderr_text.lines() {
            warn!(
                job_id = ctx.job_id,
                hook = %ctx.script_context,
                "{}", line
            );
        }
    }

    if !output.status.success() {
        anyhow::bail!(
            "{} script exited with {} (script: {})",
            ctx.script_context,
            output.status,
            script_path
        );
    }

    Ok(())
}

/// Context for the job-submission hook. Feeds env twins and the audit line;
/// `spec_json` is the fully-resolved spec sent to the script on stdin.
pub struct SubmitHookContext {
    pub spec_json: String,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub partition: String,
}

/// Whitelisted spec fields a submit hook may change. Identity, script/argv, and
/// resource-count fields are absent by construction, so a hook cannot forge them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SubmitHookChanges {
    pub qos: Option<String>,
    pub partition: Option<String>,
    pub account: Option<String>,
    pub constraint: Option<String>,
    pub comment: Option<String>,
    pub reservation: Option<String>,
    pub priority: Option<u32>,
    pub time_limit_minutes: Option<i64>,
    pub begin_time: Option<DateTime<Utc>>,
    pub gres: Option<Vec<String>>,
    pub hold: Option<bool>,
}

/// Decision returned by a job-submission hook.
#[derive(Debug)]
pub enum SubmitHookOutcome {
    Accept,
    Reject(String),
    Modify(SubmitHookChanges),
}

pub const SUBMIT_HOOK_WHITELIST: &[&str] = &[
    "qos",
    "partition",
    "account",
    "constraint",
    "comment",
    "reservation",
    "priority",
    "time_limit_minutes",
    "begin_time",
    "gres",
    "hold",
];

/// Wall-clock ceiling for a shell job_submit hook; a hung hook is killed and the
/// submission fails closed rather than stalling the controller.
const SUBMIT_HOOK_TIMEOUT_SECS: u64 = 30;
/// Max bytes captured from the hook's stdout/stderr each; a chatty hook can't
/// grow controller memory without bound.
const SUBMIT_HOOK_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Max length of the user-facing rejection reason. stderr doubles as the hook's
/// log stream, so a large or noisy stderr must not become a multi-MB gRPC status.
const SUBMIT_HOOK_MAX_REASON_BYTES: usize = 4096;

/// Reject a non-absolute hook path: a bare name would resolve via `$PATH` and
/// silently run the wrong binary. The config contract requires a fully-qualified path.
pub fn require_absolute_hook_path(script_path: &str) -> anyhow::Result<()> {
    if !std::path::Path::new(script_path).is_absolute() {
        anyhow::bail!("job_submit hook path must be absolute: {script_path}");
    }
    Ok(())
}

/// Refuse a hook the controller's account does not exclusively control: the file
/// is executed / loaded as the (root) controller and a hook-set QoS bypasses the
/// per-user ACL, so anyone who can write it gains that privilege. Require it be
/// owned by root or the controller's own uid and not group/world-writable.
#[cfg(unix)]
pub fn require_secure_hook_file(script_path: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(script_path)
        .with_context(|| format!("job_submit hook not found: {script_path}"))?;
    let euid = nix::unistd::geteuid().as_raw();
    if meta.uid() != 0 && meta.uid() != euid {
        anyhow::bail!(
            "job_submit hook must be owned by root or the controller user: {script_path}"
        );
    }
    if meta.mode() & 0o022 != 0 {
        anyhow::bail!("job_submit hook must not be group- or world-writable: {script_path}");
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn require_secure_hook_file(_script_path: &str) -> anyhow::Result<()> {
    Ok(())
}

/// Truncate an over-long hook rejection reason to roughly the last
/// [`SUBMIT_HOOK_MAX_REASON_BYTES`], keeping the tail (where a script's final
/// error line usually is) and snapping to a char boundary.
pub fn cap_hook_reason(reason: &str) -> String {
    if reason.len() <= SUBMIT_HOOK_MAX_REASON_BYTES {
        return reason.to_string();
    }
    let mut cut = reason.len() - SUBMIT_HOOK_MAX_REASON_BYTES;
    while cut < reason.len() && !reason.is_char_boundary(cut) {
        cut += 1;
    }
    format!("[reason truncated] …{}", &reason[cut..])
}

/// Run the job-submission hook: spec as JSON on stdin; non-zero exit = reject
/// (stderr to user), exit 0 blank = accept, exit 0 + JSON = modify, else `Err`.
pub async fn run_submit_hook(
    script_path: &str,
    ctx: &SubmitHookContext,
) -> anyhow::Result<SubmitHookOutcome> {
    require_absolute_hook_path(script_path)?;
    require_secure_hook_file(script_path)?;
    info!(
        target: "audit",
        hook = "job_submit",
        script = script_path,
        user = %ctx.user,
        uid = ctx.uid,
        partition = %ctx.partition,
        "running job_submit hook"
    );

    let mut env = SpurEnv::new();
    env.set_with_slurm_twin("SPUR_JOB_PARTITION", &ctx.partition);
    env.set("SPUR_JOB_USER", &ctx.user);
    env.set("SPUR_JOB_UID", ctx.uid);
    env.set("SPUR_JOB_GID", ctx.gid);
    env.set("SPUR_SCRIPT_CONTEXT", "job_submit");

    let mut cmd = Command::new(script_path);
    for (k, v) in env.into_map() {
        cmd.env(k, v);
    }
    cmd.current_dir("/tmp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("job_submit script failed to execute: {script_path}"))?;

    // Drain output concurrently with the stdin write: a multi-MB spec plus a
    // script that emits before consuming stdin would otherwise deadlock.
    let mut stdin = child
        .stdin
        .take()
        .context("job_submit stdin was not captured")?;
    let mut stdout = child
        .stdout
        .take()
        .context("job_submit stdout was not captured")?;
    let mut stderr = child
        .stderr
        .take()
        .context("job_submit stderr was not captured")?;
    let spec_bytes = ctx.spec_json.clone().into_bytes();
    let writer = async move {
        // A hook that ignores stdin closes the pipe early; a broken pipe there
        // is expected, not a failure.
        if let Err(e) = stdin.write_all(&spec_bytes).await {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(e);
            }
        }
        stdin.shutdown().await.or_else(ignore_broken_pipe)
    };

    // Drain both streams to EOF, finish the stdin write, and reap the child under
    // one deadline; read_capped keeps reading past the cap so this can't deadlock.
    let timeout = std::time::Duration::from_secs(SUBMIT_HOOK_TIMEOUT_SECS);
    let collected = tokio::time::timeout(timeout, async {
        tokio::join!(
            writer,
            read_capped(&mut stdout, SUBMIT_HOOK_MAX_OUTPUT_BYTES),
            read_capped(&mut stderr, SUBMIT_HOOK_MAX_OUTPUT_BYTES),
            child.wait(),
        )
    })
    .await;
    let (write_res, out_capped, err_capped, status) = match collected {
        Ok(tuple) => tuple,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!(
                "job_submit hook timed out after {SUBMIT_HOOK_TIMEOUT_SECS}s (script: {script_path})"
            );
        }
    };
    write_res.context("failed to write spec to job_submit stdin")?;
    let (out_bytes, out_truncated) = out_capped.context("failed to read job_submit stdout")?;
    let (err_bytes, err_truncated) = err_capped.context("failed to read job_submit stderr")?;
    let status = status.context("job_submit script failed to complete")?;

    // Overflowing the cap gets a distinct error rather than being silently
    // truncated (or masked as a timeout).
    if out_truncated || err_truncated {
        let stream = if out_truncated { "stdout" } else { "stderr" };
        anyhow::bail!(
            "job_submit hook {stream} exceeded {SUBMIT_HOOK_MAX_OUTPUT_BYTES} bytes (script: {script_path})"
        );
    }

    let stderr_text = String::from_utf8_lossy(&err_bytes);
    for line in stderr_text.lines() {
        warn!(target: "audit", hook = "job_submit", "{}", line);
    }

    if !status.success() {
        let reason = stderr_text.trim();
        let reason = if reason.is_empty() {
            format!("job rejected by job_submit hook (exit {status})")
        } else {
            cap_hook_reason(reason)
        };
        return Ok(SubmitHookOutcome::Reject(reason));
    }

    let stdout_text = String::from_utf8_lossy(&out_bytes);
    if stdout_text.trim().is_empty() {
        return Ok(SubmitHookOutcome::Accept);
    }

    let changes = parse_submit_changes(stdout_text.trim())?;
    Ok(SubmitHookOutcome::Modify(changes))
}

/// Read `reader` to EOF, returning `(bytes, truncated)`. Retains at most `cap`
/// bytes but keeps draining past it (discarding the excess) so the child never
/// blocks on a full pipe; `truncated` is set once the cap is exceeded, letting
/// the caller fail with a distinct "output too large" error instead of a hang.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 64 * 1024];
    let mut total = 0usize;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        total += n;
        if buf.len() < cap {
            let room = cap - buf.len();
            buf.extend_from_slice(&chunk[..n.min(room)]);
        }
    }
    Ok((buf, total > cap))
}

/// Parse the shell hook's stdout into whitelisted changes; malformed JSON or a
/// wrong type fails closed. Non-whitelisted keys are ignored and logged.
fn parse_submit_changes(stdout: &str) -> anyhow::Result<SubmitHookChanges> {
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(stdout).context("job_submit hook emitted unparseable JSON")?;
    changes_from_map(&map)
}

/// Type-check the whitelisted keys of a JSON object into `SubmitHookChanges`,
/// logging (not applying) non-whitelisted keys. Shared by the shell and Lua paths.
pub fn changes_from_map(
    map: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<SubmitHookChanges> {
    let mut leftover = Vec::new();
    let mut changes = SubmitHookChanges::default();
    for (key, value) in map {
        match key.as_str() {
            "qos" => changes.qos = Some(take_string(key, value)?),
            "partition" => changes.partition = Some(take_string(key, value)?),
            "account" => changes.account = Some(take_string(key, value)?),
            "constraint" => changes.constraint = Some(take_string(key, value)?),
            "comment" => changes.comment = Some(take_string(key, value)?),
            "reservation" => changes.reservation = Some(take_string(key, value)?),
            "priority" => changes.priority = Some(take_u32(key, value)?),
            "time_limit_minutes" => {
                changes.time_limit_minutes = Some(take_time_limit_minutes(value)?)
            }
            "begin_time" => changes.begin_time = Some(take_datetime(key, value)?),
            "gres" => changes.gres = Some(take_string_vec(key, value)?),
            "hold" => changes.hold = Some(take_bool(key, value)?),
            _ => leftover.push(key.clone()),
        }
    }

    if !leftover.is_empty() {
        warn!(
            target: "audit",
            hook = "job_submit",
            ignored = ?leftover,
            whitelist = ?SUBMIT_HOOK_WHITELIST,
            "job_submit hook set non-whitelisted fields; ignoring them"
        );
    }

    Ok(changes)
}

fn ignore_broken_pipe(e: std::io::Error) -> std::io::Result<()> {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(e)
    }
}

fn take_string(key: &str, value: &serde_json::Value) -> anyhow::Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("job_submit field `{key}` must be a string"))
}

fn take_bool(key: &str, value: &serde_json::Value) -> anyhow::Result<bool> {
    value
        .as_bool()
        .with_context(|| format!("job_submit field `{key}` must be a boolean"))
}

fn take_i64(key: &str, value: &serde_json::Value) -> anyhow::Result<i64> {
    // Lua arithmetic yields floats (`x / 2`), so accept a whole-valued float too.
    if let Some(f) = value.as_f64() {
        if value.as_i64().is_none() && f.fract() == 0.0 && f.is_finite() {
            return Ok(f as i64);
        }
    }
    value
        .as_i64()
        .with_context(|| format!("job_submit field `{key}` must be an integer"))
}

/// Fail closed on a walltime a hook must not set: negative (would slip past the
/// partition max-time cap) or so large it overflows `Duration::try_minutes`.
fn take_time_limit_minutes(value: &serde_json::Value) -> anyhow::Result<i64> {
    let minutes = take_i64("time_limit_minutes", value)?;
    if minutes < 0 {
        anyhow::bail!("job_submit field `time_limit_minutes` must not be negative");
    }
    if chrono::Duration::try_minutes(minutes).is_none() {
        anyhow::bail!("job_submit field `time_limit_minutes` is out of range");
    }
    Ok(minutes)
}

fn take_u32(key: &str, value: &serde_json::Value) -> anyhow::Result<u32> {
    let n = take_i64(key, value)?;
    u32::try_from(n).with_context(|| format!("job_submit field `{key}` is out of range for u32"))
}

fn take_string_vec(key: &str, value: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    let arr = value
        .as_array()
        .with_context(|| format!("job_submit field `{key}` must be an array of strings"))?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .with_context(|| format!("job_submit field `{key}` must be an array of strings"))
        })
        .collect()
}

fn take_datetime(key: &str, value: &serde_json::Value) -> anyhow::Result<DateTime<Utc>> {
    let s = take_string(key, value)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .with_context(|| format!("job_submit field `{key}` must be an RFC3339 timestamp"))
}

/// Apply whitelisted hook changes onto the spec, returning the names of the
/// fields actually changed (for the audit line). Only `Some` fields are touched.
pub fn apply_submit_changes(spec: &mut JobSpec, changes: &SubmitHookChanges) -> Vec<&'static str> {
    let mut modified = Vec::new();
    // Empty is ignored for these three: blanking them would skip the existence /
    // ACL checks that treat an empty value as "unset, nothing to validate".
    if let Some(qos) = changes.qos.as_deref().filter(|s| !s.is_empty()) {
        spec.qos = Some(qos.to_string());
        modified.push("qos");
    }
    if let Some(partition) = changes.partition.as_deref().filter(|s| !s.is_empty()) {
        spec.partition = Some(partition.to_string());
        modified.push("partition");
    }
    if let Some(account) = changes.account.as_deref().filter(|s| !s.is_empty()) {
        spec.account = Some(account.to_string());
        modified.push("account");
    }
    if let Some(ref constraint) = changes.constraint {
        spec.constraint = Some(constraint.clone());
        modified.push("constraint");
    }
    if let Some(ref comment) = changes.comment {
        spec.comment = Some(comment.clone());
        modified.push("comment");
    }
    if let Some(ref reservation) = changes.reservation {
        spec.reservation = Some(reservation.clone());
        modified.push("reservation");
    }
    if let Some(priority) = changes.priority {
        spec.priority = Some(priority);
        modified.push("priority");
    }
    // Validated in `take_time_limit_minutes`; `try_minutes` keeps this panic-free
    // even if that guard is ever bypassed.
    if let Some(dur) = changes
        .time_limit_minutes
        .and_then(chrono::Duration::try_minutes)
    {
        spec.time_limit = Some(dur);
        modified.push("time_limit");
    }
    if let Some(begin_time) = changes.begin_time {
        spec.begin_time = Some(begin_time);
        modified.push("begin_time");
    }
    if let Some(ref gres) = changes.gres {
        spec.gres = gres.clone();
        modified.push("gres");
    }
    if let Some(hold) = changes.hold {
        spec.hold = hold;
        modified.push("hold");
    }
    modified
}

/// Spawn a hook in `work_dir`, retrying from `/tmp` if the spawn fails there.
/// A missing/untraversable `work_dir` must not fail the hook (spurd drains the
/// node on hook failure); only a failure that also persists from `/tmp` is real.
fn spawn_hook_in_work_dir(
    cmd: &mut Command,
    work_dir: &str,
    job_id: JobId,
    script_context: &str,
) -> std::io::Result<tokio::process::Child> {
    if work_dir.is_empty() {
        return cmd.current_dir("/tmp").spawn();
    }
    let first_err = match cmd.current_dir(work_dir).spawn() {
        Ok(child) => return Ok(child),
        Err(e) => e,
    };
    let child = cmd.current_dir("/tmp").spawn()?;
    warn!(
        job_id,
        hook = %script_context,
        work_dir,
        error = %first_err,
        "hook could not start in work_dir, ran from /tmp instead"
    );
    Ok(child)
}

fn resolve_username(uid: u32) -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| uid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_script(body: &str) -> tempfile::TempPath {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "#!/bin/bash\n{}", body).unwrap();
        let path = f.into_temp_path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    fn test_ctx() -> HookContext {
        HookContext {
            job_id: 42,
            work_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            partition: "gpu".into(),
            nodelist: "node01".into(),
            script_context: "prolog_slurmd".into(),
            gpu_devices: vec![0, 1],
            cpus: 8,
            memory_mb: 16384,
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_success() {
        let script = make_script("exit 0");
        let ctx = test_ctx();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_failure_returns_error() {
        let script = make_script("exit 1");
        let ctx = test_ctx();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("prolog_slurmd"));
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_nonexistent_script() {
        let ctx = test_ctx();
        let result = run_hook("/nonexistent/hook.sh", &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_receives_env_vars() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let body = format!(
            "echo \"$SPUR_JOB_ID|$SPUR_JOB_UID|$SPUR_JOB_PARTITION|$SPUR_SCRIPT_CONTEXT|$SPUR_JOB_GPUS|$SPUR_CPUS_ON_NODE\" > {}",
            marker_path
        );
        let script = make_script(&body);
        let ctx = test_ctx();
        run_hook(script.to_str().unwrap(), &ctx).await.unwrap();

        let content = std::fs::read_to_string(&marker_path).unwrap();
        let parts: Vec<&str> = content.trim().split('|').collect();
        assert_eq!(parts[0], "42");
        assert_eq!(parts[1], ctx.uid.to_string());
        assert_eq!(parts[2], "gpu");
        assert_eq!(parts[3], "prolog_slurmd");
        assert_eq!(parts[4], "0,1");
        assert_eq!(parts[5], "8");
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_receives_slurm_twins() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let body = format!(
            "echo \"$SLURM_JOB_ID|$SLURM_JOB_PARTITION|$SLURM_JOB_NODELIST|$SLURM_CPUS_ON_NODE\" > {}",
            marker_path
        );
        let script = make_script(&body);
        let ctx = test_ctx();
        run_hook(script.to_str().unwrap(), &ctx).await.unwrap();

        let content = std::fs::read_to_string(&marker_path).unwrap();
        let parts: Vec<&str> = content.trim().split('|').collect();
        assert_eq!(parts[0], "42");
        assert_eq!(parts[1], "gpu");
        assert_eq!(parts[2], "node01");
        assert_eq!(parts[3], "8");
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_stderr_does_not_prevent_success() {
        let script = make_script("echo 'warning message' >&2\nexit 0");
        let ctx = test_ctx();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        assert!(result.is_ok());
    }

    // A missing work_dir must not fail the hook (spurd would drain the node).
    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_with_missing_work_dir_still_runs() {
        let script = make_script("exit 0");
        let mut ctx = test_ctx();
        ctx.work_dir = "/nonexistent/path/that/does/not/exist".into();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        assert!(result.is_ok());
    }

    // An existing-but-untraversable work_dir (NFS root_squash) also fails to
    // spawn, which a stat-based precheck would miss. Root bypasses perm checks.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_with_untraversable_work_dir_still_runs() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let script = make_script("exit 0");
        let mut ctx = test_ctx();
        ctx.work_dir = dir.path().to_string_lossy().into_owned();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_runs_in_work_dir_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let marker = NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let body = format!("pwd -P > {}", marker_path);
        let script = make_script(&body);
        let mut ctx = test_ctx();
        ctx.work_dir = canonical.to_string_lossy().into_owned();
        run_hook(script.to_str().unwrap(), &ctx).await.unwrap();
        let content = std::fs::read_to_string(&marker_path).unwrap();
        assert_eq!(content.trim(), canonical.to_string_lossy());
    }

    // SPUR_JOB_WORK_DIR still reports the submitted path when the CWD falls back.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_reports_submitted_work_dir_when_cwd_falls_back() {
        let marker = NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let body = format!("echo \"$SPUR_JOB_WORK_DIR|$(pwd)\" > {}", marker_path);
        let script = make_script(&body);
        let mut ctx = test_ctx();
        ctx.work_dir = "/nonexistent/submitted/dir".into();
        run_hook(script.to_str().unwrap(), &ctx).await.unwrap();

        let content = std::fs::read_to_string(&marker_path).unwrap();
        let parts: Vec<&str> = content.trim().split('|').collect();
        assert_eq!(parts[0], "/nonexistent/submitted/dir");
        assert_eq!(parts[1], "/tmp");
    }

    // A genuinely broken script still errors — the /tmp retry doesn't mask it.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_missing_script_still_errors_after_fallback() {
        let mut ctx = test_ctx();
        ctx.work_dir = "/nonexistent/submitted/dir".into();
        let result = run_hook("/nonexistent/hook_script.sh", &ctx).await;
        assert!(result.is_err());
    }

    fn submit_ctx() -> SubmitHookContext {
        SubmitHookContext {
            spec_json: r#"{"name":"j1","partition":"gpu"}"#.into(),
            user: "alice".into(),
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            partition: "gpu".into(),
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_accept() {
        let script = make_script("exit 0");
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        assert!(matches!(out, SubmitHookOutcome::Accept));
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_reject_surfaces_stderr() {
        let script = make_script("echo 'partition required' >&2\nexit 1");
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Reject(msg) => assert_eq!(msg, "partition required"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_reject_empty_stderr_gets_generic_message() {
        let script = make_script("exit 3");
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Reject(msg) => assert!(msg.contains("job_submit hook")),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_modify_whitelisted_field() {
        let script = make_script(r#"echo '{"qos":"high"}'"#);
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.qos.as_deref(), Some("high")),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_ignores_non_whitelisted_field() {
        let script = make_script(r#"echo '{"uid":0,"qos":"high"}'"#);
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.qos.as_deref(), Some("high")),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    // A hook that exits 0 but emits garbage is a misconfiguration; fail closed
    // rather than silently accept and bypass enforcement.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_malformed_json_fails_closed() {
        let script = make_script("echo '{not json'");
        let result = run_submit_hook(script.to_str().unwrap(), &submit_ctx()).await;
        assert!(result.is_err());
    }

    // A whitelisted key with the wrong type also fails closed.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_wrong_type_fails_closed() {
        let script = make_script(r#"echo '{"priority":"high"}'"#);
        let result = run_submit_hook(script.to_str().unwrap(), &submit_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_receives_spec_on_stdin() {
        let marker = NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let script = make_script(&format!("cat > {marker_path}"));
        run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        let content = std::fs::read_to_string(&marker_path).unwrap();
        assert!(content.contains("\"partition\":\"gpu\""));
    }

    // A hook that never reads stdin closes the pipe early; the broken-pipe write
    // must be tolerated, not surfaced as a failure.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_that_ignores_stdin_still_succeeds() {
        let script = make_script(r#"echo '{"qos":"high"}'"#);
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.qos.as_deref(), Some("high")),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    #[test]
    fn apply_changes_sets_all_whitelisted_and_leaves_identity() {
        let mut spec = JobSpec {
            user: "alice".into(),
            uid: 1000,
            script: Some("run.sh".into()),
            num_nodes: 4,
            ..Default::default()
        };
        let begin = DateTime::parse_from_rfc3339("2026-08-03T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let changes = SubmitHookChanges {
            qos: Some("high".into()),
            partition: Some("gpu".into()),
            account: Some("research".into()),
            constraint: Some("mi300x".into()),
            comment: Some("audited".into()),
            reservation: Some("resv1".into()),
            priority: Some(500),
            time_limit_minutes: Some(120),
            begin_time: Some(begin),
            gres: Some(vec!["gpu:mi300x:4".into()]),
            hold: Some(true),
        };
        let modified = apply_submit_changes(&mut spec, &changes);

        assert_eq!(spec.qos.as_deref(), Some("high"));
        assert_eq!(spec.partition.as_deref(), Some("gpu"));
        assert_eq!(spec.account.as_deref(), Some("research"));
        assert_eq!(spec.constraint.as_deref(), Some("mi300x"));
        assert_eq!(spec.comment.as_deref(), Some("audited"));
        assert_eq!(spec.reservation.as_deref(), Some("resv1"));
        assert_eq!(spec.priority, Some(500));
        assert_eq!(spec.time_limit, Some(chrono::Duration::minutes(120)));
        assert_eq!(spec.begin_time, Some(begin));
        assert_eq!(spec.gres, vec!["gpu:mi300x:4".to_string()]);
        assert!(spec.hold);
        for field in [
            "qos",
            "partition",
            "account",
            "constraint",
            "comment",
            "reservation",
            "priority",
            "time_limit",
            "begin_time",
            "gres",
            "hold",
        ] {
            assert!(modified.contains(&field), "missing {field}");
        }

        assert_eq!(spec.user, "alice");
        assert_eq!(spec.uid, 1000);
        assert_eq!(spec.script.as_deref(), Some("run.sh"));
        assert_eq!(spec.num_nodes, 4);
    }

    #[test]
    fn apply_changes_ignores_empty_partition_qos_account() {
        let mut spec = JobSpec {
            partition: Some("gpu".into()),
            qos: Some("high".into()),
            account: Some("acct".into()),
            ..Default::default()
        };
        let changes = SubmitHookChanges {
            partition: Some(String::new()),
            qos: Some(String::new()),
            account: Some(String::new()),
            ..Default::default()
        };
        let modified = apply_submit_changes(&mut spec, &changes);
        assert!(modified.is_empty());
        assert_eq!(spec.partition.as_deref(), Some("gpu"));
        assert_eq!(spec.qos.as_deref(), Some("high"));
        assert_eq!(spec.account.as_deref(), Some("acct"));
    }

    #[test]
    fn parse_changes_handles_all_typed_fields() {
        let json = r#"{
            "priority": 7,
            "time_limit_minutes": 90,
            "begin_time": "2026-08-03T12:00:00Z",
            "gres": ["gpu:mi300x:2"],
            "hold": true
        }"#;
        let c = parse_submit_changes(json).unwrap();
        assert_eq!(c.priority, Some(7));
        assert_eq!(c.time_limit_minutes, Some(90));
        assert!(c.begin_time.is_some());
        assert_eq!(c.gres.as_deref(), Some(&["gpu:mi300x:2".to_string()][..]));
        assert_eq!(c.hold, Some(true));
    }

    #[test]
    fn time_limit_minutes_rejects_negative_and_huge() {
        assert!(parse_submit_changes(r#"{"time_limit_minutes": -1}"#).is_err());
        assert!(parse_submit_changes(r#"{"time_limit_minutes": 9223372036854775807}"#).is_err());
        // A sane positive value is still accepted.
        let c = parse_submit_changes(r#"{"time_limit_minutes": 60}"#).unwrap();
        assert_eq!(c.time_limit_minutes, Some(60));
    }

    #[test]
    fn hook_paths_must_be_absolute() {
        assert!(require_absolute_hook_path("job_submit.sh").is_err());
        assert!(require_absolute_hook_path("./rel/path.sh").is_err());
        assert!(require_absolute_hook_path("/etc/spur/job_submit.sh").is_ok());
    }

    // A hung hook is killed at the wall-clock deadline and fails closed, rather
    // than stalling submission forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(run_hooks)]
    async fn submit_hook_timeout_fails_closed() {
        let script = make_script("sleep 60");
        let err = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .expect_err("a hung hook must fail, not hang");
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    // Overflowing the output cap is reported as a distinct "too large" error, not
    // masked as a generic timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(run_hooks)]
    async fn submit_hook_output_cap_is_distinct_error() {
        // Emit ~2 MiB to stdout, over the 1 MiB cap, then exit 0.
        let script = make_script("head -c 2097152 /dev/zero | tr '\\0' 'a'\nexit 0");
        let err = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .expect_err("output past the cap must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeded"),
            "expected a size error, got: {msg}"
        );
        assert!(!msg.contains("timed out"), "must not be a timeout: {msg}");
    }

    #[test]
    fn cap_hook_reason_truncates_long_reason() {
        let short = "denied: bad partition";
        assert_eq!(cap_hook_reason(short), short);
        let long = "x".repeat(SUBMIT_HOOK_MAX_REASON_BYTES * 2);
        let capped = cap_hook_reason(&long);
        assert!(capped.len() < long.len());
        assert!(capped.starts_with("[reason truncated]"));
    }

    // A group/world-writable hook is an arbitrary-code / QoS-escalation surface
    // and must be refused. Root bypasses Unix perm checks, so skip as root.
    #[cfg(unix)]
    #[test]
    fn secure_hook_file_rejects_world_writable() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "#!/bin/bash\nexit 0").unwrap();
        let path = f.into_temp_path();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = require_secure_hook_file(path.to_str().unwrap())
            .expect_err("world-writable hook must be refused");
        assert!(err.to_string().contains("writable"), "got: {err}");
        // A tightened mode is accepted.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(require_secure_hook_file(path.to_str().unwrap()).is_ok());
    }
}
