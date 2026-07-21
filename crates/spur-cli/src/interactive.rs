// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::{interactive_input, interactive_output, InitSession, InteractiveInput};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};

/// Connect to a spurd agent, applying the standard gRPC size limits.
pub async fn connect_agent(addr: &str) -> Result<SlurmAgentClient<tonic::transport::Channel>> {
    let channel = spur_client::connect_channel(addr)
        .await
        .context("cannot connect to agent")?;
    Ok(SlurmAgentClient::new(channel)
        .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
        .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE))
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
    agent: &mut SlurmAgentClient<tonic::transport::Channel>,
    job_id: u32,
    step_id: u32,
    argv: Vec<String>,
    winsize: spur_proto::proto::WindowSize,
    overlap: bool,
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

    Ok(exit_code)
}

/// Run a full interactive PTY session over the InteractiveSession RPC.
/// Returns the remote exit code.
pub async fn run_interactive_session(
    agent: &mut SlurmAgentClient<tonic::transport::Channel>,
    job_id: u32,
    step_id: u32,
    argv: Vec<String>,
    winsize: spur_proto::proto::WindowSize,
    overlap: bool,
) -> Result<i32> {
    let handle = open_interactive_session(agent, job_id, step_id, argv, winsize, overlap)
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
