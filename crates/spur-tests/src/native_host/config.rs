// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use anyhow::{bail, Result};

/// Native-host test configuration from environment variables.
#[derive(Debug, Clone)]
pub struct TestConfig {
    pub nodes: Vec<String>,
    pub binaries_dir: PathBuf,
    pub ssh_key: Option<PathBuf>,
    pub ssh_user: String,
}

impl TestConfig {
    pub fn from_env() -> Result<Self> {
        let nodes = std::env::var("SPUR_TEST_BM_NODES")
            .unwrap_or_else(|_| "localhost".into())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        if nodes.is_empty() {
            bail!("SPUR_TEST_BM_NODES must list at least one host");
        }

        let binaries_dir = std::env::var("SPUR_TEST_BM_BINARIES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.join("target/release"))
                    .unwrap_or_else(|| PathBuf::from("target/release"))
            });

        let ssh_key = std::env::var("SPUR_TEST_BM_SSH_KEY")
            .ok()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());

        let ssh_user = std::env::var("SPUR_TEST_BM_SSH_USER").unwrap_or_else(|_| {
            std::env::var("USER")
                .or_else(|_| std::env::var("LOGNAME"))
                .unwrap_or_else(|_| "root".into())
        });

        Ok(Self {
            nodes,
            binaries_dir,
            ssh_key,
            ssh_user,
        })
    }

    pub fn binary_path(&self, name: &str) -> PathBuf {
        self.binaries_dir.join(name)
    }

    pub fn validate_binaries(&self) -> Result<()> {
        for name in ["spurctld", "spurd", "spur"] {
            let path = self.binary_path(name);
            if !path.is_file() {
                bail!(
                    "missing binary {} (set SPUR_TEST_BM_BINARIES_DIR or run cargo build --release)",
                    path.display()
                );
            }
        }
        Ok(())
    }
}

/// Override remote work directory. In CI, set via workflow env to a
/// deterministic path based on run ID. Locally, leave unset — the fixture
/// generates `/tmp/spur-bm-{pid}-{timestamp}` automatically.
pub fn remote_dir_override() -> Option<String> {
    std::env::var("SPUR_TEST_BM_REMOTE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Derive a unique remote directory path for this test run.
/// Mirrors the K8s namespace pattern (`spur-ci-{pid}-{ts}`).
pub fn default_remote_dir() -> String {
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("/tmp/spur-bm-{pid}-{ts}")
}

pub fn controller_port() -> u16 {
    std::env::var("SPUR_TEST_BM_CONTROLLER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6817)
}

pub fn agent_port() -> u16 {
    std::env::var("SPUR_TEST_BM_AGENT_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6818)
}

pub fn resolve_controller_url(host: &str) -> String {
    format!("http://{}:{}", host, controller_port())
}
