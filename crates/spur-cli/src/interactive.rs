// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{
    interactive_input, interactive_output, GetJobRequest, InitSession, InteractiveInput,
    JobKeepaliveRequest,
};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};

/// Keeps an interactive allocation attended by pinging the controller on a
/// fixed interval, and stops the pings when dropped. Aborting on `Drop` means
/// an early `?` return on the caller's path can't leak the task.
pub struct KeepaliveGuard(tokio::task::JoinHandle<()>);

impl Drop for KeepaliveGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn the keepalive loop for `job_id`. `tool` prefixes the warning printed
/// when a ping fails (e.g. "salloc", "srun"). A blocking client sends no other
/// traffic, so without these pings the controller's InactiveLimit reaper would
/// reclaim a live allocation.
pub fn spawn_keepalive(
    client: SlurmControllerClient<crate::authclient::AuthChannel>,
    job_id: u32,
    user: String,
    tool: &'static str,
) -> KeepaliveGuard {
    let handle = tokio::spawn(async move {
        let mut client = client;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(
            spur_core::config::KEEPALIVE_INTERVAL_SECS,
        ));
        // Warn once per failure streak: a persistent failure stays visible
        // without printing a line every interval.
        let mut warned = false;
        loop {
            tick.tick().await;
            match client
                .job_keepalive(JobKeepaliveRequest {
                    job_id,
                    user: user.clone(),
                })
                .await
            {
                Ok(_) => warned = false,
                Err(e) if !warned => {
                    eprintln!(
                        "{tool}: warning: keepalive to controller failed ({}); \
                         allocation may be reaped if this persists",
                        e.message()
                    );
                    warned = true;
                }
                Err(_) => {}
            }
        }
    });
    KeepaliveGuard(handle)
}

/// Connect to a spurd agent, presenting the caller's credential if one is available.
///
/// The agent authenticates callers with the same JWT mechanism as the controller. A user token
/// from `$SPUR_AUTH_TOKEN` / `~/.spur/token` is signed with the cluster key and will be accepted.
/// Without a token the connection still succeeds against agents in `permissive` mode, but will be
/// refused in `required` mode.
pub async fn connect_agent(addr: &str) -> Result<SlurmAgentClient<crate::authclient::AuthChannel>> {
    let channel = spur_client::connect_channel(addr)
        .await
        .context("cannot connect to agent")?;
    Ok(SlurmAgentClient::new(crate::authclient::wrap(channel))
        .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
        .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE))
}

/// Local username sent with authenticated job requests.
///
/// Refuse to continue when the operating system cannot resolve the caller:
/// recording a sentinel can either collide with a real account or disagree
/// with a later exec/attach request.
pub fn current_user() -> Result<String> {
    whoami::username().context("failed to determine current username")
}

/// Username to send with step and keepalive RPCs for a job in the current shell.
///
/// Prefer the allocation owner exported by salloc (`SPUR_JOB_USER` / `SLURM_JOB_USER`)
/// when `SPUR_JOB_ID` / `SLURM_JOB_ID` matches *job_id*; otherwise use *known_owner*
/// when the caller already fetched the job, then `GetJob`; fall back to the local login name.
pub async fn job_caller_user(
    client: &mut SlurmControllerClient<crate::authclient::AuthChannel>,
    job_id: u32,
    known_owner: Option<&str>,
) -> Result<String> {
    if let Some(user) = allocation_env_job_user(job_id) {
        return Ok(user);
    }
    if let Some(owner) = known_owner.map(str::trim).filter(|o| !o.is_empty()) {
        return Ok(owner.to_string());
    }
    let resp = client
        .get_job(GetJobRequest { job_id })
        .await
        .context("failed to look up job owner for step RPC")?;
    let user = resp.into_inner().user;
    if !user.is_empty() {
        return Ok(user);
    }
    current_user()
}

/// Owner from allocation env vars when the exported job id matches *job_id*.
fn allocation_env_job_user(job_id: u32) -> Option<String> {
    let env_job_id = std::env::var("SPUR_JOB_ID")
        .or_else(|_| std::env::var("SLURM_JOB_ID"))
        .ok()?;
    if env_job_id.trim().parse::<u32>().ok()? != job_id {
        return None;
    }
    let user = std::env::var("SPUR_JOB_USER")
        .or_else(|_| std::env::var("SLURM_JOB_USER"))
        .ok()?;
    let user = user.trim().to_string();
    (!user.is_empty()).then_some(user)
}

/// Username for cancel RPCs when submit-time ``whoami`` may differ from the
/// controller's bound owner (for example after JWT submit).
pub async fn resolve_job_owner_for_cancel(
    client: &mut SlurmControllerClient<crate::authclient::AuthChannel>,
    job_id: u32,
    submit_user: &str,
) -> String {
    match client.get_job(GetJobRequest { job_id }).await {
        Ok(resp) => {
            let owner = resp.into_inner().user;
            if owner.is_empty() {
                submit_user.to_string()
            } else {
                owner
            }
        }
        Err(_) => submit_user.to_string(),
    }
}

/// Propagate the caller's auth token into a child process (allocation shell).
pub fn inherit_auth_token(cmd: &mut tokio::process::Command) {
    if let Ok(token) = std::env::var("SPUR_AUTH_TOKEN") {
        if !token.trim().is_empty() {
            cmd.env("SPUR_AUTH_TOKEN", token);
            return;
        }
    }
    if let Some(token) = crate::authclient::load_token() {
        cmd.env("SPUR_AUTH_TOKEN", token);
    }
}

pub fn get_terminal_size() -> spur_proto::proto::WindowSize {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    spur_proto::proto::WindowSize {
        rows: rows as u32,
        cols: cols as u32,
        xpixel: 0,
        ypixel: 0,
    }
}

/// Established interactive session: the input sender and output stream.
pub struct InteractiveSessionHandle {
    pub in_tx: tokio::sync::mpsc::Sender<InteractiveInput>,
    pub out_stream: tonic::Streaming<spur_proto::proto::InteractiveOutput>,
}

/// Open the InteractiveSession RPC, returning the raw handle.
///
/// Returns `Err(tonic::Status)` on RPC failure.
pub async fn open_interactive_session(
    agent: &mut SlurmAgentClient<crate::authclient::AuthChannel>,
    job_id: u32,
    step_id: u32,
    argv: Vec<String>,
    winsize: spur_proto::proto::WindowSize,
    overlap: bool,
    user: &str,
) -> std::result::Result<InteractiveSessionHandle, tonic::Status> {
    let init = InteractiveInput {
        msg: Some(interactive_input::Msg::Init(InitSession {
            job_id,
            step_id,
            overlap,
            pty: true,
            winsize: Some(winsize),
            argv,
            env: HashMap::new(),
            user: user.to_string(),
        })),
    };

    let (in_tx, in_rx) = tokio::sync::mpsc::channel::<InteractiveInput>(64);
    in_tx.send(init).await.ok();

    let in_stream = tokio_stream::wrappers::ReceiverStream::new(in_rx);
    let response = agent.interactive_session(in_stream).await?;

    Ok(InteractiveSessionHandle {
        in_tx,
        out_stream: response.into_inner(),
    })
}

/// Drive the I/O loop for an already-opened interactive session.
/// Returns the remote exit code.
pub async fn drive_interactive_session(handle: InteractiveSessionHandle) -> Result<i32> {
    let InteractiveSessionHandle {
        in_tx,
        mut out_stream,
    } = handle;

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        prev_hook(info);
    }));

    let _raw_guard = match RawModeGuard::enter() {
        Ok(g) => Some(g),
        Err(_) => {
            eprintln!("spur: warning: raw mode unavailable (stdin is not a TTY)");
            None
        }
    };

    let mut sigwinch = signal(SignalKind::window_change())?;

    let mut stdout = tokio::io::stdout();
    let mut stdin = tokio::io::stdin();
    let mut stdin_buf = vec![0u8; 4096];
    let mut stdin_open = true;
    let mut in_tx = Some(in_tx);

    let exit_code: i32 = loop {
        tokio::select! {
            msg = out_stream.message() => {
                match msg {
                    Ok(Some(output)) => {
                        match output.msg {
                            Some(interactive_output::Msg::Data(data)) => {
                                stdout.write_all(&data).await?;
                                stdout.flush().await?;
                            }
                            Some(interactive_output::Msg::ExitStatus(code)) => {
                                break code;
                            }
                            None => {}
                        }
                    }
                    Ok(None) => break 1,
                    Err(e) => {
                        eprintln!("\r\nstream error: {e}");
                        break 1;
                    }
                }
            }

            n = stdin.read(&mut stdin_buf), if stdin_open => {
                match n {
                    Ok(0) => {
                        stdin_open = false;
                        in_tx.take();
                    }
                    Ok(n) => {
                        if let Some(ref tx) = in_tx {
                            let _ = tx.send(InteractiveInput {
                                msg: Some(interactive_input::Msg::Stdin(
                                    stdin_buf[..n].to_vec(),
                                )),
                            }).await;
                        }
                    }
                    Err(_) => {
                        stdin_open = false;
                        in_tx.take();
                    }
                }
            }

            _ = sigwinch.recv(), if stdin_open => {
                let ws = get_terminal_size();
                if let Some(ref tx) = in_tx {
                    let _ = tx.send(InteractiveInput {
                        msg: Some(interactive_input::Msg::Resize(ws)),
                    }).await;
                }
            }
        }
    };

    drop(_raw_guard);
    let _ = std::panic::take_hook(); // remove our raw-mode panic hook

    Ok(exit_code)
}

/// Run a full interactive PTY session over the InteractiveSession RPC.
/// Returns the remote exit code.
pub async fn run_interactive_session(
    agent: &mut SlurmAgentClient<crate::authclient::AuthChannel>,
    job_id: u32,
    step_id: u32,
    argv: Vec<String>,
    winsize: spur_proto::proto::WindowSize,
    overlap: bool,
    user: &str,
) -> Result<i32> {
    let handle = open_interactive_session(agent, job_id, step_id, argv, winsize, overlap, user)
        .await
        .map_err(|status| anyhow::anyhow!("InteractiveSession RPC failed: {}", status.message()))?;
    drive_interactive_session(handle).await
}

/// RAII guard that puts the terminal into raw mode and restores it on drop.
pub struct RawModeGuard;

impl RawModeGuard {
    pub fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_defaults::EnvGuard;
    use crate::mock_controller;
    use serial_test::serial;
    use tonic::Code;

    #[tokio::test]
    #[serial(env_injection)]
    async fn job_caller_user_prefers_spur_job_user_env() {
        let _env = EnvGuard::new();
        std::env::set_var("SPUR_JOB_ID", "42");
        std::env::set_var("SPUR_JOB_USER", "jwt-owner");
        let (addr, capture) = mock_controller::spawn().await;
        let mut client = mock_controller::client(addr).await;
        let user = job_caller_user(&mut client, 42, None)
            .await
            .expect("env owner");
        assert_eq!(user, "jwt-owner");
        assert_eq!(capture.get_job_calls(), 0);
    }

    #[tokio::test]
    #[serial(env_injection)]
    async fn job_caller_user_slurm_job_user_alias() {
        let _env = EnvGuard::new();
        std::env::set_var("SLURM_JOB_ID", "42");
        std::env::set_var("SLURM_JOB_USER", "slurm-owner");
        let (addr, capture) = mock_controller::spawn().await;
        let mut client = mock_controller::client(addr).await;
        let user = job_caller_user(&mut client, 42, None)
            .await
            .expect("slurm env owner");
        assert_eq!(user, "slurm-owner");
        assert_eq!(capture.get_job_calls(), 0);
    }

    #[tokio::test]
    #[serial(env_injection)]
    async fn job_caller_user_ignores_stale_allocation_env() {
        let _env = EnvGuard::new();
        std::env::set_var("SPUR_JOB_ID", "1");
        std::env::set_var("SPUR_JOB_USER", "stale-owner");
        let (addr, capture) = mock_controller::spawn().await;
        let mut client = mock_controller::client(addr).await;
        let user = job_caller_user(&mut client, 99, Some("controller-owner"))
            .await
            .expect("known owner");
        assert_eq!(user, "controller-owner");
        assert_eq!(capture.get_job_calls(), 0);
    }

    #[tokio::test]
    #[serial(env_injection)]
    async fn job_caller_user_uses_known_owner_without_get_job() {
        let _env = EnvGuard::new();
        let (addr, capture) = mock_controller::spawn().await;
        let mut client = mock_controller::client(addr).await;
        let user = job_caller_user(&mut client, 7, Some("controller-owner"))
            .await
            .expect("known owner");
        assert_eq!(user, "controller-owner");
        assert_eq!(capture.get_job_calls(), 0);
    }

    #[tokio::test]
    #[serial(env_injection)]
    async fn job_caller_user_fetches_owner_from_get_job() {
        let _env = EnvGuard::new();
        let (addr, capture) = mock_controller::spawn().await;
        capture.set_get_job_user("from-controller");
        let mut client = mock_controller::client(addr).await;
        let user = job_caller_user(&mut client, 9, None)
            .await
            .expect("get_job owner");
        assert_eq!(user, "from-controller");
        assert_eq!(capture.get_job_calls(), 1);
    }

    #[tokio::test]
    #[serial(env_injection)]
    async fn job_caller_user_propagates_get_job_failure() {
        let _env = EnvGuard::new();
        let (addr, capture) = mock_controller::spawn().await;
        capture.set_get_job_error(Code::Unavailable);
        let mut client = mock_controller::client(addr).await;
        let err = job_caller_user(&mut client, 9, None)
            .await
            .expect_err("get_job failure must not fall back to whoami");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to look up job owner"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    #[serial(env_injection)]
    async fn resolve_job_owner_for_cancel_uses_controller_owner() {
        let _env = EnvGuard::new();
        let (addr, capture) = mock_controller::spawn().await;
        capture.set_get_job_user("bound-owner");
        let mut client = mock_controller::client(addr).await;
        let user = resolve_job_owner_for_cancel(&mut client, 3, "submit-wire-name").await;
        assert_eq!(user, "bound-owner");
    }

    #[tokio::test]
    #[serial(env_injection)]
    async fn resolve_job_owner_for_cancel_falls_back_on_get_job_error() {
        let _env = EnvGuard::new();
        let (addr, capture) = mock_controller::spawn().await;
        capture.set_get_job_error(Code::Unavailable);
        let mut client = mock_controller::client(addr).await;
        let user = resolve_job_owner_for_cancel(&mut client, 3, "submit-wire-name").await;
        assert_eq!(user, "submit-wire-name");
    }

    #[test]
    #[serial(env_injection)]
    fn inherit_auth_token_exports_env_token() {
        let _env = EnvGuard::new();
        std::env::set_var("SPUR_AUTH_TOKEN", "test-jwt-token");
        let mut cmd = tokio::process::Command::new("/bin/true");
        inherit_auth_token(&mut cmd);
        let envs: std::collections::HashMap<_, _> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|s| s.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs.get("SPUR_AUTH_TOKEN").and_then(|v| v.as_deref()),
            Some("test-jwt-token")
        );
    }
}
