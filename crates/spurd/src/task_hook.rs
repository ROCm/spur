// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Task-hook execution. `TaskProlog`/`TaskEpilog` run as the job user inside the
//! step's cgroup leaf, honoring the Slurm TaskProlog `export`/`unset`/`print`
//! stdout protocol. Unlike node prolog/epilog (root, outside the cgroup), these
//! are contained and can shape the task environment.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;

use anyhow::Context;
use tokio::io::AsyncReadExt;
use tracing::{info, warn};

use spur_core::hooks::{secure_hook_command, spawn_hook_in_work_dir, HookContext};

use crate::executor::CgroupJoin;

/// Max bytes captured from a task hook's stdout/stderr each; a chatty hook cannot
/// grow the supervisor's memory without bound.
const TASK_HOOK_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// A single TaskProlog stdout directive.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskPrologDirective {
    Export(String, String),
    Unset(String),
    Print(String),
}

/// The outcome of a TaskProlog run: the (possibly mutated) task environment and
/// any `print` text to prepend to the task's own stdout.
#[derive(Debug)]
pub(crate) struct TaskPrologResult {
    pub(crate) environment: HashMap<String, String>,
    pub(crate) printed: Vec<u8>,
}

/// A POSIX-ish environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse TaskProlog stdout. `export NAME=value` (value may contain `=`),
/// `unset NAME`, and `print ...` are directives; a recognized-but-malformed
/// directive is an error, and any other line is warned about and ignored.
fn parse_task_prolog_output(output: &str) -> anyhow::Result<Vec<TaskPrologDirective>> {
    let mut directives = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("export ") {
            let (name, value) = rest
                .split_once('=')
                .with_context(|| format!("TaskProlog `export` without `=`: {line}"))?;
            if !is_valid_env_name(name) {
                anyhow::bail!("TaskProlog `export` has an invalid variable name: {name}");
            }
            directives.push(TaskPrologDirective::Export(
                name.to_string(),
                value.to_string(),
            ));
        } else if let Some(rest) = line.strip_prefix("unset ") {
            let name = rest.trim();
            if !is_valid_env_name(name) {
                anyhow::bail!("TaskProlog `unset` has an invalid variable name: {name}");
            }
            directives.push(TaskPrologDirective::Unset(name.to_string()));
        } else if let Some(rest) = line.strip_prefix("print ") {
            directives.push(TaskPrologDirective::Print(rest.to_string()));
        } else if line.trim().is_empty() {
            continue;
        } else {
            warn!(line, "ignoring unrecognized TaskProlog output line");
        }
    }
    Ok(directives)
}

struct TaskHookOutput {
    stdout: Vec<u8>,
}

/// Read `reader` to EOF, keeping at most `cap` bytes but draining the rest so the
/// child never blocks on a full pipe. Returns `(bytes, truncated)`.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
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

/// Run a task hook as the job user, joined to the step cgroup leaf. Captures and
/// caps stdout/stderr while draining both, logs stderr, and fails on a non-zero
/// exit or a capped overflow.
async fn run_task_hook(
    script_path: &str,
    context: &HookContext,
    task_environment: &HashMap<String, String>,
    cgroup_path: Option<&Path>,
) -> anyhow::Result<TaskHookOutput> {
    info!(
        job_id = context.job_id,
        hook = %context.script_context,
        script = script_path,
        "running task hook"
    );
    // Built parent-side: nothing between fork and exec may allocate.
    let mut secure = secure_hook_command(script_path)?;
    let cgroup_join = CgroupJoin::for_cgroup(cgroup_path);
    let priv_drop = crate::privdrop::PrivDrop::resolve_if_needed(context.uid, context.gid);

    {
        let cmd = secure.command_mut();
        cmd.env_clear();
        for (k, v) in task_environment {
            cmd.env(k, v);
        }
        for (k, v) in context.environment() {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Join while still root (a dropped process cannot write cgroup.procs), then
        // drop privilege.
        // SAFETY: the closure performs only async-signal-safe calls.
        unsafe {
            cmd.pre_exec(move || {
                if let Some(ref join) = cgroup_join {
                    join.join_required()?;
                }
                if let Some(ref pd) = priv_drop {
                    pd.apply()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                }
                Ok(())
            });
        }
    }

    // Run in the job's work dir (with run_hook's /tmp fallback) so the hook's
    // relative paths and $PWD match the task, not the daemon's inherited cwd.
    let mut child = spawn_hook_in_work_dir(
        secure.command_mut(),
        &context.work_dir,
        context.job_id,
        &context.script_context,
    )
    .with_context(|| format!("task hook failed to execute: {script_path}"))?;
    let mut stdout = child
        .stdout
        .take()
        .context("task hook stdout was not captured")?;
    let mut stderr = child
        .stderr
        .take()
        .context("task hook stderr was not captured")?;
    // Drain both streams and reap concurrently; read_capped keeps reading past
    // the cap so a chatty hook cannot deadlock on a full pipe.
    let (out, err, status) = tokio::join!(
        read_capped(&mut stdout, TASK_HOOK_MAX_OUTPUT_BYTES),
        read_capped(&mut stderr, TASK_HOOK_MAX_OUTPUT_BYTES),
        child.wait(),
    );
    // `secure` (holding the validated fd) is still alive here.
    let (out_bytes, out_truncated) = out.context("failed to read task hook stdout")?;
    let (err_bytes, err_truncated) = err.context("failed to read task hook stderr")?;
    let status = status.context("task hook failed to complete")?;

    for line in String::from_utf8_lossy(&err_bytes).lines() {
        warn!(
            job_id = context.job_id,
            hook = %context.script_context,
            "{}", line
        );
    }
    if out_truncated || err_truncated {
        anyhow::bail!(
            "task hook output exceeded {TASK_HOOK_MAX_OUTPUT_BYTES} bytes: {script_path}"
        );
    }
    if !status.success() {
        anyhow::bail!("task hook exited with {status}: {script_path}");
    }
    Ok(TaskHookOutput { stdout: out_bytes })
}

/// Run `TaskProlog` and apply its `export`/`unset`/`print` protocol to
/// `task_environment`, returning the final environment and any printed bytes.
pub(crate) async fn run_task_prolog(
    script_path: &str,
    context: &HookContext,
    mut task_environment: HashMap<String, String>,
    cgroup_path: Option<&Path>,
) -> anyhow::Result<TaskPrologResult> {
    let output = run_task_hook(script_path, context, &task_environment, cgroup_path).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut printed = Vec::new();
    for directive in parse_task_prolog_output(&text)? {
        match directive {
            TaskPrologDirective::Export(name, value) => {
                task_environment.insert(name, value);
            }
            TaskPrologDirective::Unset(name) => {
                task_environment.remove(&name);
            }
            TaskPrologDirective::Print(line) => {
                printed.extend_from_slice(line.as_bytes());
                printed.push(b'\n');
            }
        }
    }
    Ok(TaskPrologResult {
        environment: task_environment,
        printed,
    })
}

/// Run `TaskEpilog`. Its stdout is not an environment protocol — it cannot mutate
/// an already-finished task's environment.
pub(crate) async fn run_task_epilog(
    script_path: &str,
    context: &HookContext,
    task_environment: &HashMap<String, String>,
    cgroup_path: Option<&Path>,
) -> anyhow::Result<()> {
    run_task_hook(script_path, context, task_environment, cgroup_path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn make_script(body: &str) -> tempfile::TempPath {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "#!/bin/bash\n{body}").unwrap();
        let path = f.into_temp_path();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn test_ctx() -> HookContext {
        HookContext {
            job_id: 7,
            work_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            partition: "batch".into(),
            nodelist: "node01".into(),
            script_context: "prolog_task".into(),
            gpu_devices: vec![],
            cpus: 1,
            memory_mb: 128,
        }
    }

    #[test]
    fn parses_the_slurm_task_prolog_protocol() {
        let parsed =
            parse_task_prolog_output("export FOO=bar=baz\nunset OLD\nprint ready now\n").unwrap();
        assert_eq!(
            parsed,
            vec![
                TaskPrologDirective::Export("FOO".into(), "bar=baz".into()),
                TaskPrologDirective::Unset("OLD".into()),
                TaskPrologDirective::Print("ready now".into()),
            ]
        );
    }

    #[test]
    fn rejects_invalid_environment_names() {
        assert!(parse_task_prolog_output("export 1BAD=x\n").is_err());
        assert!(parse_task_prolog_output("unset A=B\n").is_err());
    }

    #[test]
    fn ignores_blank_and_unknown_lines() {
        assert!(parse_task_prolog_output("\nordinary diagnostic\n")
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn task_prolog_exports_unsets_and_prints() {
        let script =
            make_script("echo 'export FOO=bar'\necho 'unset OLD'\necho 'print hello world'");
        let mut env = HashMap::new();
        env.insert("OLD".to_string(), "x".to_string());
        env.insert("KEEP".to_string(), "y".to_string());
        let result = run_task_prolog(script.to_str().unwrap(), &test_ctx(), env, None)
            .await
            .unwrap();
        assert_eq!(
            result.environment.get("FOO").map(String::as_str),
            Some("bar")
        );
        assert!(!result.environment.contains_key("OLD"));
        assert_eq!(
            result.environment.get("KEEP").map(String::as_str),
            Some("y")
        );
        assert_eq!(result.printed, b"hello world\n");
    }

    #[tokio::test]
    async fn task_prolog_nonzero_exit_is_an_error() {
        let script = make_script("exit 1");
        let result =
            run_task_prolog(script.to_str().unwrap(), &test_ctx(), HashMap::new(), None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn task_epilog_nonzero_exit_is_returned_to_its_caller() {
        let script = make_script("exit 2");
        let result =
            run_task_epilog(script.to_str().unwrap(), &test_ctx(), &HashMap::new(), None).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_prolog_output_over_the_cap_fails_without_deadlock() {
        // ~2 MiB to stdout, over the 1 MiB cap, then exit 0.
        let script = make_script("head -c 2097152 /dev/zero | tr '\\0' 'a'\nexit 0");
        let err = run_task_prolog(script.to_str().unwrap(), &test_ctx(), HashMap::new(), None)
            .await
            .expect_err("output past the cap must fail");
        assert!(err.to_string().contains("exceeded"), "got: {err}");
    }

    // A plain temp dir stands in for the step cgroup: the pre_exec join writes the
    // child's pid into cgroup.procs, proving the join ran before exec.
    #[tokio::test]
    async fn task_hook_joins_the_provided_cgroup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup.procs"), "").unwrap();
        let script = make_script("true");
        run_task_epilog(
            script.to_str().unwrap(),
            &test_ctx(),
            &HashMap::new(),
            Some(dir.path()),
        )
        .await
        .unwrap();
        let procs = std::fs::read_to_string(dir.path().join("cgroup.procs")).unwrap();
        let pid: i32 = procs.trim().parse().unwrap_or(0);
        assert!(
            pid > 0,
            "the hook's pid should have been written to cgroup.procs, got: {procs:?}"
        );
    }

    #[tokio::test]
    async fn task_hook_runs_in_the_job_work_dir() {
        // Without a cwd the hook would run from the daemon's inherited directory; it
        // must run in the job's work_dir so $PWD and relative paths match the task.
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let script = make_script("echo \"print $(pwd -P)\"");
        let mut ctx = test_ctx();
        ctx.work_dir = canonical.to_string_lossy().into_owned();
        let result = run_task_prolog(script.to_str().unwrap(), &ctx, HashMap::new(), None)
            .await
            .unwrap();
        let printed = String::from_utf8(result.printed).unwrap();
        assert_eq!(printed.trim(), canonical.to_string_lossy());
    }
}
