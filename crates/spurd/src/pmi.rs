// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Minimal PMI-1 wire protocol server for MPI rank bootstrap.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Notify;
use tracing::{debug, warn};

pub struct PmiServer {
    socket_path: String,
    num_ranks: u32,
    kvs: Arc<Mutex<HashMap<String, String>>>,
    barrier_count: Arc<std::sync::atomic::AtomicU32>,
    barrier_notify: Arc<Notify>,
    // Signals `run`'s accept loop to exit so the bound listener fd and the
    // accept task are released, not just the socket path. `notify_one` stores a
    // permit, so a shutdown raised between loop iterations is not lost.
    shutdown: Arc<Notify>,
}

impl PmiServer {
    pub fn new(socket_path: &str, num_ranks: u32) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            num_ranks,
            kvs: Arc::new(Mutex::new(HashMap::new())),
            barrier_count: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            barrier_notify: Arc::new(Notify::new()),
            shutdown: Arc::new(Notify::new()),
        }
    }

    pub async fn run(&self) {
        let listener = match UnixListener::bind(&self.socket_path) {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %e, "PMI server bind failed");
                return;
            }
        };

        loop {
            let (stream, _) = tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(s) => s,
                    Err(_) => break,
                },
                _ = self.shutdown.notified() => break,
            };
            let kvs = self.kvs.clone();
            let num_ranks = self.num_ranks;
            let barrier_count = self.barrier_count.clone();
            let barrier_notify = self.barrier_notify.clone();

            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();

                while let Ok(Some(line)) = lines.next_line().await {
                    let response =
                        handle_pmi_command(&line, &kvs, num_ranks, &barrier_count, &barrier_notify)
                            .await;
                    if writer.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    }

    pub fn cleanup(&self) {
        self.shutdown.notify_one();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn handle_pmi_command(
    line: &str,
    kvs: &Arc<Mutex<HashMap<String, String>>>,
    num_ranks: u32,
    barrier_count: &Arc<std::sync::atomic::AtomicU32>,
    barrier_notify: &Arc<Notify>,
) -> String {
    // Parse key=value pairs from line
    let mut params = HashMap::new();
    for part in line.split_whitespace() {
        if let Some((k, v)) = part.split_once('=') {
            params.insert(k, v);
        }
    }

    match params.get("cmd").copied() {
        Some("init") => "cmd=response_to_init pmi_version=1 pmi_subversion=1 rc=0\n".to_string(),
        Some("get_maxes") => {
            "cmd=maxes kvsname_max=256 keylen_max=256 vallen_max=256\n".to_string()
        }
        Some("get_appnum") => "cmd=appnum appnum=0\n".to_string(),
        Some("get_my_kvsname") => "cmd=my_kvsname kvsname=spur_kvs\n".to_string(),
        Some("get_universe_size") => {
            format!("cmd=universe_size size={}\n", num_ranks)
        }
        Some("put") => {
            if let (Some(key), Some(val)) = (params.get("key"), params.get("value")) {
                kvs.lock().unwrap().insert(key.to_string(), val.to_string());
                debug!(key, val, "PMI put");
                "cmd=put_result rc=0\n".to_string()
            } else {
                "cmd=put_result rc=-1 msg=missing_key_or_value\n".to_string()
            }
        }
        Some("get") => {
            if let Some(key) = params.get("key") {
                let val = kvs.lock().unwrap().get(*key).cloned().unwrap_or_default();
                format!("cmd=get_result rc=0 value={}\n", val)
            } else {
                "cmd=get_result rc=-1 msg=missing_key\n".to_string()
            }
        }
        Some("barrier_in") => {
            let count = barrier_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if count >= num_ranks {
                barrier_count.store(0, std::sync::atomic::Ordering::SeqCst);
                barrier_notify.notify_waiters();
            } else {
                barrier_notify.notified().await;
            }
            "cmd=barrier_out\n".to_string()
        }
        Some("finalize") => "cmd=finalize_ack\n".to_string(),
        _ => "cmd=error rc=-1 msg=unknown_command\n".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pmi_init_response() {
        let kvs = Arc::new(Mutex::new(HashMap::new()));
        let bc = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let bn = Arc::new(Notify::new());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = rt.block_on(handle_pmi_command("cmd=init", &kvs, 4, &bc, &bn));
        assert!(resp.contains("pmi_version=1"));
    }

    #[test]
    fn test_pmi_put_get() {
        let kvs = Arc::new(Mutex::new(HashMap::new()));
        let bc = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let bn = Arc::new(Notify::new());
        let rt = tokio::runtime::Runtime::new().unwrap();

        let resp = rt.block_on(handle_pmi_command(
            "cmd=put key=test_key value=test_val",
            &kvs,
            4,
            &bc,
            &bn,
        ));
        assert!(resp.contains("rc=0"));

        let resp = rt.block_on(handle_pmi_command(
            "cmd=get key=test_key",
            &kvs,
            4,
            &bc,
            &bn,
        ));
        assert!(resp.contains("value=test_val"));
    }

    #[test]
    fn test_pmi_appnum() {
        let kvs = Arc::new(Mutex::new(HashMap::new()));
        let bc = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let bn = Arc::new(Notify::new());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = rt.block_on(handle_pmi_command("cmd=get_appnum", &kvs, 4, &bc, &bn));
        assert!(resp.contains("appnum=0"));
    }

    // cleanup() must stop run()'s accept loop, not just unlink the socket, so
    // the bound listener fd and the accept task are released. Awaiting the run
    // handle is deterministic: it only resolves once the loop has exited.
    #[tokio::test]
    async fn cleanup_stops_accept_loop() {
        let socket_path = format!("/tmp/spur-pmi-shutdown-{}.sock", std::process::id());
        let _ = std::fs::remove_file(&socket_path);
        let server = Arc::new(PmiServer::new(&socket_path, 2));

        let run_server = server.clone();
        let handle = tokio::spawn(async move { run_server.run().await });

        // The server is live once run() has bound the listener. The spawn above
        // may not have reached bind yet, so retry the connect (yielding, no fixed
        // sleep) until it succeeds — proving the accept loop is actually up.
        let mut connected = false;
        for _ in 0..1000 {
            if tokio::net::UnixStream::connect(&socket_path).await.is_ok() {
                connected = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(connected, "PMI server never became connectable");

        server.cleanup();

        // run() must return now that shutdown was signalled; if the loop still
        // blocked on accept(), this await would hang.
        handle.await.expect("run task must exit after cleanup");
        assert!(
            !std::path::Path::new(&socket_path).exists(),
            "cleanup must also remove the socket file"
        );
    }
}
