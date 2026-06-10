// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::Stdio;

use anyhow::Context;
use tokio::process::Command;
use tracing::{info, warn};

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

    let child = Command::new(script_path)
        .env("SPUR_JOB_ID", ctx.job_id.to_string())
        .env("SPUR_JOB_USER", &username)
        .env("SPUR_JOB_UID", ctx.uid.to_string())
        .env("SPUR_JOB_GID", ctx.gid.to_string())
        .env("SPUR_JOB_WORK_DIR", &ctx.work_dir)
        .env("SPUR_JOB_PARTITION", &ctx.partition)
        .env("SPUR_JOB_NODELIST", &ctx.nodelist)
        .env("SPUR_JOB_GPUS", &gpu_list)
        .env("SPUR_CPUS_ON_NODE", ctx.cpus.to_string())
        .env("SPUR_JOB_MEMORY_MB", ctx.memory_mb.to_string())
        .env("SPUR_SCRIPT_CONTEXT", &ctx.script_context)
        .current_dir(&ctx.work_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
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
    async fn hook_stderr_does_not_prevent_success() {
        let script = make_script("echo 'warning message' >&2\nexit 0");
        let ctx = test_ctx();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        assert!(result.is_ok());
    }
}
