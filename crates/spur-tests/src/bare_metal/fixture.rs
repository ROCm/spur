// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use tracing::info;

use super::config::{
    agent_port, controller_port, default_remote_dir, remote_dir_override, resolve_controller_url,
    TestConfig,
};
use super::ssh::SshNode;

const BM_BINARIES: [&str; 3] = ["spurctld", "spurd", "spur"];

/// Deployed bare-metal cluster for E2E tests.
pub struct BareMetalFixture {
    pub config: TestConfig,
    pub nodes: Vec<SshNode>,
    pub node_names: Vec<String>,
    pub remote_dir: String,
    pub controller_addr: String,
    pub bin_dir: String,
    pub etc_dir: String,
    pub state_dir: String,
    pub log_dir: String,
    pub ssh_user: String,
}

impl BareMetalFixture {
    pub async fn deploy(config: TestConfig) -> Result<Self> {
        config.validate_binaries()?;
        info!("deploy: validated binaries");

        let ssh_key = config.ssh_key.as_deref();
        let mut nodes = Vec::new();
        let mut node_names = Vec::new();

        for host in &config.nodes {
            info!(host, "connecting via SSH");
            let node = SshNode::connect(host, &config.ssh_user, ssh_key).await?;
            info!(host, "killing stale processes");
            node.kill_processes("spurctld").await?;
            node.kill_processes("spurd").await?;
            nodes.push(node);
        }

        let remote_dir = remote_dir_override().unwrap_or_else(default_remote_dir);
        info!(remote_dir, "remote workdir");

        let bin_dir = format!("{remote_dir}/bin");
        let etc_dir = format!("{remote_dir}/etc");
        let state_dir = format!("{remote_dir}/state");
        let log_dir = format!("{remote_dir}/log");

        info!("creating directories on all nodes");
        for node in &nodes {
            node.rm_rf(&remote_dir).await?;
            node.mkdir_p(&remote_dir).await?;
            node.mkdir_p(&bin_dir).await?;
            node.mkdir_p(&etc_dir).await?;
            node.mkdir_p(&state_dir).await?;
            node.mkdir_p(&log_dir).await?;
        }

        for name in BM_BINARIES {
            info!(name, "uploading binary to all nodes");
            let local = config.binary_path(name);
            for node in &nodes {
                let remote = format!("{bin_dir}/{name}");
                node.scp_to(&local, &remote).await?;
                node.exec(&format!("chmod +x '{remote}'")).await?;
            }
        }

        info!("creating CLI symlinks");
        for node in &nodes {
            node.exec(&format!(
                "cd '{bin_dir}' && for cmd in sbatch srun squeue scancel sinfo scontrol; do ln -sf spur $cmd 2>/dev/null || true; done"
            ))
            .await?;
        }

        for node in &nodes {
            let name = node
                .exec("hostname -s")
                .await
                .unwrap_or_else(|_| node.host.clone())
                .trim()
                .to_string();
            node_names.push(name);
        }

        let controller_host = &config.nodes[0];
        let controller_addr = if controller_host == "localhost" || controller_host == "127.0.0.1" {
            format!("http://127.0.0.1:{}", controller_port())
        } else {
            resolve_controller_url(controller_host)
        };

        info!("writing spur.conf");
        let spur_conf = generate_spur_conf(&node_names);
        nodes[0]
            .write_remote_file(&format!("{etc_dir}/spur.conf"), spur_conf.as_bytes())
            .await?;

        info!(node = %node_names[0], "starting spurctld");
        let listen = format!("[::]:{}", controller_port());
        let ctld_cmd = format!(
            "nohup '{bin_dir}/spurctld' -f '{etc_dir}/spur.conf' \
             --listen '{listen}' --state-dir '{state_dir}' --log-level info -D \
             > '{log_dir}/spurctld.log' 2>&1 & echo $!"
        );
        let ctld_pid = nodes[0].exec(&ctld_cmd).await?;
        info!(pid = ctld_pid.trim(), "spurctld started");

        tokio::time::sleep(Duration::from_secs(2)).await;

        for (i, node) in nodes.iter().enumerate() {
            let hostname = &node_names[i];
            let ctrl = controller_addr.clone();
            let address = if node.host == "localhost" || node.host == "127.0.0.1" {
                "127.0.0.1".to_string()
            } else {
                node.host.clone()
            };
            let agent_listen = format!("0.0.0.0:{}", agent_port());
            info!(hostname, "starting spurd");
            let spurd_cmd = format!(
                "nohup '{bin_dir}/spurd' --controller '{ctrl}' --listen '{agent_listen}' \
                 --hostname '{hostname}' --address '{address}' --log-level info -D \
                 > '{log_dir}/spurd.log' 2>&1 & echo $!"
            );
            let spurd_pid = node.exec(&spurd_cmd).await?;
            info!(hostname, pid = spurd_pid.trim(), "spurd started");
        }

        let fixture = Self {
            config: config.clone(),
            nodes,
            node_names,
            remote_dir: remote_dir.clone(),
            controller_addr,
            bin_dir: bin_dir.clone(),
            etc_dir,
            state_dir,
            log_dir,
            ssh_user: config.ssh_user.clone(),
        };

        info!("waiting for all nodes to become idle");
        fixture.wait_all_idle(Duration::from_secs(120)).await?;
        info!(nodes = ?fixture.node_names, "bare-metal cluster ready");
        Ok(fixture)
    }

    pub fn controller(&self) -> &SshNode {
        &self.nodes[0]
    }

    pub fn node_by_name(&self, name: &str) -> Option<&SshNode> {
        self.node_names
            .iter()
            .position(|n| n == name)
            .map(|i| &self.nodes[i])
    }

    pub async fn cli(&self, args: &[&str]) -> Result<String> {
        if args.is_empty() {
            bail!("cli requires at least one argument");
        }
        let mut cmd = format!(
            "SPUR_CONTROLLER_ADDR='{}' PATH='{}':$PATH",
            self.controller_addr, self.bin_dir
        );
        cmd.push(' ');
        cmd.push_str(&format!("'{}/{}'", self.bin_dir, args[0]));
        for arg in &args[1..] {
            cmd.push(' ');
            cmd.push_str(&shell_escape(arg));
        }
        self.controller().exec(&cmd).await
    }

    pub async fn sbatch(&self, args: &[&str]) -> Result<String> {
        let mut a = vec!["sbatch"];
        a.extend(args);
        self.cli(&a).await
    }

    pub async fn squeue_all(&self) -> Result<String> {
        self.cli(&["squeue", "-t", "all"]).await
    }

    pub async fn sinfo(&self) -> Result<String> {
        self.cli(&["sinfo"]).await
    }

    pub async fn scancel(&self, job_id: &str) -> Result<String> {
        self.cli(&["scancel", job_id]).await
    }

    pub async fn scontrol_release(&self, job_id: &str) -> Result<String> {
        self.cli(&["scontrol", "release", job_id]).await
    }

    pub async fn write_script(&self, name: &str, body: &str) -> Result<String> {
        let path = format!("{}/{}", self.remote_dir, name);
        self.controller()
            .write_remote_file(&path, body.as_bytes())
            .await?;
        self.controller()
            .exec(&format!("chmod +x '{path}'"))
            .await?;
        Ok(path)
    }

    pub async fn job_stdout_path(&self, job_id: u32) -> Option<String> {
        let out = self
            .cli(&["scontrol", "show", "job", &job_id.to_string()])
            .await
            .ok()?;
        for token in out.split_whitespace() {
            if let Some(path) = token.strip_prefix("StdOut=") {
                if !path.is_empty() {
                    return Some(path.to_string());
                }
            }
        }
        None
    }

    pub async fn read_job_output(&self, job_id: u32) -> Result<String> {
        if let Some(path) = self.job_stdout_path(job_id).await {
            let content = self.read_output_on_any_node(&path).await?;
            if !content.trim().is_empty() {
                return Ok(content);
            }
        }
        Ok(String::new())
    }

    pub async fn read_output_on_any_node(&self, path: &str) -> Result<String> {
        if let Ok(content) = self.controller().read_remote_file(path).await {
            if !content.trim().is_empty() {
                return Ok(content);
            }
        }
        for node in self.nodes.iter().skip(1) {
            if let Ok(content) = node.read_remote_file(path).await {
                if !content.trim().is_empty() {
                    return Ok(content);
                }
            }
        }
        Ok(String::new())
    }

    pub fn cluster_is_ready(&self, sinfo_output: &str) -> bool {
        let all_registered = self
            .node_names
            .iter()
            .all(|name| sinfo_output.contains(name));
        if !all_registered {
            return false;
        }
        for line in sinfo_output.lines() {
            if !line.contains("idle") {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            for window in fields.windows(2) {
                if window[1] == "idle" {
                    if let Ok(n) = window[0].parse::<usize>() {
                        return n >= self.node_names.len();
                    }
                }
            }
        }
        false
    }

    pub async fn wait_all_idle(&self, timeout: Duration) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            let out = self.sinfo().await.unwrap_or_default();
            if self.cluster_is_ready(&out) {
                return Ok(());
            }
            if start.elapsed() > timeout {
                bail!(
                    "timeout waiting for {} idle nodes; sinfo:\n{out}",
                    self.node_names.len()
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn teardown(&self) -> Result<()> {
        for node in &self.nodes {
            node.kill_processes("spurctld").await?;
            node.kill_processes("spurd").await?;
            node.rm_rf(&self.remote_dir).await?;
        }
        info!("bare-metal cluster torn down");
        Ok(())
    }

    pub async fn ship_file_to_all_agents(&self, local: &Path, remote_name: &str) -> Result<()> {
        for node in &self.nodes {
            let remote = format!("{}/{}", self.remote_dir, remote_name);
            node.scp_to(local, &remote).await?;
        }
        Ok(())
    }

    pub async fn ship_bytes_to_all_agents(&self, remote_name: &str, data: &[u8]) -> Result<()> {
        for node in &self.nodes {
            let remote = format!("{}/{}", self.remote_dir, remote_name);
            node.write_remote_file(&remote, data).await?;
        }
        Ok(())
    }

    /// Verify that at least `min_nodes` agents have visible GPU hardware.
    /// Panics with a diagnostic message if the check fails.
    pub async fn gpu_preflight(&self, min_nodes: usize) {
        let mut gpu_nodes = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            let probe = node
                .exec_allow_fail(
                    "{ ls /dev/kfd 2>/dev/null && echo HAS_GPU; } || \
                     { nvidia-smi -L 2>/dev/null && echo HAS_GPU; } || \
                     echo NO_GPU",
                )
                .await
                .unwrap_or_default();
            if probe.contains("HAS_GPU") {
                gpu_nodes.push(self.node_names[i].as_str());
            }
        }
        assert!(
            gpu_nodes.len() >= min_nodes,
            "GPU preflight failed: need {min_nodes} GPU node(s), found {} ({:?}). \
             Are you running these tests on a cluster without GPUs?",
            gpu_nodes.len(),
            gpu_nodes
        );
    }

    /// Ship GPU test assets to all agents: Python scripts, HIP source,
    /// compiled gpu_test binary, ephemeral venv with PyTorch, and generated
    /// job wrapper scripts that activate the venv.
    pub async fn ship_gpu_assets(&self) {
        let deploy = bare_metal_deploy_dir();
        let rd = &self.remote_dir;

        // Ship Python test scripts
        for name in ["distributed_test.py", "inference_test.py"] {
            let local = deploy.join(name);
            if local.is_file() {
                self.ship_file_to_all_agents(&local, name)
                    .await
                    .expect(name);
            }
        }

        // Generate job wrappers that point to the ephemeral venv + remote_dir
        let dist_wrapper = format!(
            "#!/bin/bash\nsource '{rd}/venv/bin/activate'\nexec python3 '{rd}/distributed_test.py'\n"
        );
        self.ship_bytes_to_all_agents("distributed_job.sh", dist_wrapper.as_bytes())
            .await
            .expect("distributed_job.sh");
        for node in &self.nodes {
            let _ = node
                .exec(&format!("chmod +x '{rd}/distributed_job.sh'"))
                .await;
        }

        let infer_wrapper = format!(
            "#!/bin/bash\nsource '{rd}/venv/bin/activate'\nexec python3 '{rd}/inference_test.py'\n"
        );
        self.ship_bytes_to_all_agents("inference_job.sh", infer_wrapper.as_bytes())
            .await
            .expect("inference_job.sh");
        for node in &self.nodes {
            let _ = node
                .exec(&format!("chmod +x '{rd}/inference_job.sh'"))
                .await;
        }

        // Compile HIP gpu_test binary if source and toolchain are available
        let hip = deploy.join("gpu_test.hip");
        if hip.is_file() {
            self.ship_file_to_all_agents(&hip, "gpu_test.hip")
                .await
                .ok();
            for node in &self.nodes {
                let remote_src = format!("{rd}/gpu_test.hip");
                let _ = node
                    .exec(&format!(
                        "command -v hipcc >/dev/null && \
                         hipcc -o '{rd}/gpu_test' '{remote_src}' 2>/dev/null || true"
                    ))
                    .await;
            }
        }

        // Create ephemeral venv with PyTorch on each node
        let torch_index = std::env::var("SPUR_TEST_BM_TORCH_INDEX")
            .unwrap_or_else(|_| "https://download.pytorch.org/whl/rocm6.3".into());
        for (i, node) in self.nodes.iter().enumerate() {
            let name = &self.node_names[i];
            info!(name, "creating GPU venv");
            node.exec(&format!("python3 -m venv '{rd}/venv'"))
                .await
                .unwrap_or_else(|e| {
                    panic!("venv creation failed on {name} — is python3-venv installed? {e}")
                });
            info!(name, "installing torch");
            node.exec(&format!(
                "'{rd}/venv/bin/pip' install --quiet torch --index-url '{torch_index}'"
            ))
            .await
            .unwrap_or_else(|e| panic!("pip install torch failed on {name}: {e}"));
        }
    }

    /// Collect diagnostic info for a failed job (scontrol, sinfo, squeue,
    /// stdout, controller and agent logs). Best-effort — never fails.
    pub async fn debug_job(&self, job_id: u32) -> String {
        let mut d = format!("=== DEBUG job {job_id} ===\n");

        if let Ok(info) = self
            .cli(&["scontrol", "show", "job", &job_id.to_string()])
            .await
        {
            d.push_str("scontrol show job:\n");
            d.push_str(&info);
            d.push('\n');
        }

        if let Ok(info) = self.sinfo().await {
            d.push_str("sinfo:\n");
            d.push_str(&info);
            d.push('\n');
        }

        if let Ok(info) = self.squeue_all().await {
            d.push_str("squeue:\n");
            d.push_str(&info);
            d.push('\n');
        }

        if let Some(path) = self.job_stdout_path(job_id).await {
            let content = self
                .read_output_on_any_node(&path)
                .await
                .unwrap_or_default();
            if !content.trim().is_empty() {
                d.push_str(&format!("stdout ({path}):\n"));
                d.push_str(&content);
                d.push('\n');
            }
        }

        let ctrl_log = format!("{}/spurctld.log", self.log_dir);
        if let Ok(log) = self
            .controller()
            .exec_allow_fail(&format!("tail -30 '{ctrl_log}'"))
            .await
        {
            d.push_str(&format!(
                "spurctld.log on {} (last 30 lines):\n",
                self.node_names[0]
            ));
            d.push_str(&log);
            d.push('\n');
        }

        for (i, node) in self.nodes.iter().enumerate() {
            let agent_log = format!("{}/spurd.log", self.log_dir);
            if let Ok(log) = node
                .exec_allow_fail(&format!("tail -15 '{agent_log}'"))
                .await
            {
                d.push_str(&format!(
                    "spurd.log on {} (last 15 lines):\n",
                    self.node_names[i]
                ));
                d.push_str(&log);
                d.push('\n');
            }
        }

        d
    }

    /// Read output for a given path from **all** nodes and return combined text.
    pub async fn read_output_all_nodes(&self, path: &str) -> String {
        let mut all = String::new();
        for node in &self.nodes {
            if let Ok(content) = node.read_remote_file(path).await {
                if !content.trim().is_empty() {
                    if !all.is_empty() {
                        all.push('\n');
                    }
                    all.push_str(&content);
                }
            }
        }
        all
    }

    /// Build a minimal squashfs container image locally, then SCP to all nodes.
    ///
    /// The image contains bash, coreutils, and NSS libs — enough to run
    /// shell scripts, resolve DNS, and exercise namespace isolation.
    /// Returns the remote path to the `.sqsh` file.
    pub async fn build_container_image(&self) -> Result<String> {
        let remote_path = format!("{}/test-container.sqsh", self.remote_dir);

        let local_tmp = tempfile::TempDir::new()?;
        let rootfs = local_tmp.path().join("rootfs");
        let local_img = local_tmp.path().join("test-container.sqsh");

        let build_script = format!(
            r#"set -e
R='{rootfs}'
mkdir -p "$R/bin" "$R/usr/bin" "$R/lib" "$R/lib64" \
  "$R/etc" "$R/dev" "$R/proc" "$R/sys" "$R/tmp" \
  "$R/run" "$R/home" "$R/mnt"
for b in bash cat echo sleep hostname id df env stat ls wc head tail tr touch mkdir getent; do
  src=$(which "$b" 2>/dev/null) || continue
  [ -f "$src" ] && cp "$src" "$R/usr/bin/"
done
ln -sf /usr/bin/bash "$R/bin/bash"
ln -sf /usr/bin/bash "$R/bin/sh"
for f in "$R/usr/bin/"*; do
  ldd "$f" 2>/dev/null | grep '=>' | awk '{{print $3}}' | while read -r lib; do
    if [ -f "$lib" ]; then
      dir=$(dirname "$lib")
      mkdir -p "$R$dir"
      cp -n "$lib" "$R$lib" 2>/dev/null || true
    fi
  done
done
for nsslib in libnss_dns.so.2 libnss_files.so.2 libresolv.so.2; do
  src="/lib/x86_64-linux-gnu/$nsslib"
  [ -f "$src" ] && mkdir -p "$R/lib/x86_64-linux-gnu" && cp -n "$src" "$R/lib/x86_64-linux-gnu/" 2>/dev/null || true
done
[ -f /lib64/ld-linux-x86-64.so.2 ] && cp -n /lib64/ld-linux-x86-64.so.2 "$R/lib64/" 2>/dev/null || true
for f in /etc/passwd /etc/group /etc/nsswitch.conf; do
  [ -f "$f" ] && cp "$f" "$R/etc/"
done
mksquashfs "$R" '{img}' -noappend -quiet >/dev/null 2>&1
"#,
            rootfs = rootfs.display(),
            img = local_img.display(),
        );

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&build_script)
            .output()
            .await?;
        if !output.status.success() {
            bail!(
                "local mksquashfs failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        info!(
            "built container image locally ({} bytes)",
            std::fs::metadata(&local_img)?.len()
        );

        for (i, node) in self.nodes.iter().enumerate() {
            node.scp_to(&local_img, &remote_path).await?;
            info!("shipped container image to {}", self.node_names[i]);
        }

        Ok(remote_path)
    }

    /// Check container prerequisites. Panics with actionable hints if anything
    /// is missing.
    ///
    /// Checks: mksquashfs on runner, unsquashfs on nodes, and that rootless
    /// user namespaces work (AppArmor restriction on Ubuntu 24.04+).
    pub async fn container_preflight(&self) {
        assert!(
            std::process::Command::new("mksquashfs")
                .arg("--help")
                .output()
                .is_ok(),
            "mksquashfs not found on test runner. Install:\n  sudo apt install squashfs-tools"
        );

        for (i, node) in self.nodes.iter().enumerate() {
            let probe = node
                .exec_allow_fail("command -v unsquashfs >/dev/null && echo OK || echo MISSING")
                .await
                .unwrap_or_default();
            assert!(
                probe.contains("OK"),
                "unsquashfs not found on {}. Install:\n  sudo apt install squashfs-tools",
                self.node_names[i]
            );

            let restrict = node
                .exec_allow_fail(
                    "sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0",
                )
                .await
                .unwrap_or_default();
            // If restriction is active, check that an AppArmor profile is loaded for spurd
            if restrict.trim() == "1" {
                let has_profile = node
                    .exec_allow_fail(
                        "sudo aa-status 2>/dev/null | grep -q 'spur-' && echo OK || echo MISSING",
                    )
                    .await
                    .unwrap_or_default();
                if !has_profile.contains("OK") {
                    let spurd_path = format!("{}/bin/spurd", self.remote_dir);
                    panic!(
                        "Container tests need user namespace access on {node}, but \
                         kernel.apparmor_restrict_unprivileged_userns=1 is active \
                         and no AppArmor profile is loaded for spurd.\n\n\
                         Fix with ONE of:\n\n\
                         Option A — Disable the restriction (recommended for test nodes):\n  \
                           ssh {user}@{host} 'sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0'\n  \
                           # Persist across reboots:\n  \
                           ssh {user}@{host} 'echo kernel.apparmor_restrict_unprivileged_userns=0 \
                           | sudo tee /etc/sysctl.d/99-spur-userns.conf'\n\n\
                         Option B — Load an AppArmor profile for spurd:\n  \
                           ssh {user}@{host} \"echo 'abi <abi/4.0>,\n  \
                           profile spur-test {spurd_path} flags=(unconfined) {{\n    \
                           userns,\n  \
                           }}' | sudo apparmor_parser -r\"",
                        node = self.node_names[i],
                        spurd_path = spurd_path,
                        user = self.ssh_user,
                        host = node.host,
                    );
                }
            }
        }
    }
}

fn generate_spur_conf(node_names: &[String]) -> String {
    let nodes_list = node_names.join(",");
    let mut nodes_toml = String::new();
    for name in node_names {
        nodes_toml.push_str(&format!(
            r#"
[[nodes]]
names = "{name}"
cpus = 64
memory_mb = 262144
"#
        ));
    }

    format!(
        r#"cluster_name = "bare-metal-ci"

[scheduler]
interval_secs = 1
plugin = "backfill"

[auth]
plugin = "none"

[[partitions]]
name = "default"
state = "UP"
default = true
nodes = "{nodes_list}"
max_time = "24:00:00"
default_time = "10:00"

[network]
wg_enabled = false
agent_port = {agent_port}
{nodes_toml}
"#,
        nodes_list = nodes_list,
        agent_port = agent_port(),
        nodes_toml = nodes_toml
    )
}

fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-:".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

pub fn parse_job_id(sbatch_output: &str) -> Option<u32> {
    sbatch_output
        .split_whitespace()
        .last()
        .and_then(|s| s.parse().ok())
}

/// Path to deploy/bare-metal scripts (respects `SPUR_TEST_BM_DEPLOY_DIR`).
pub fn bare_metal_deploy_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("SPUR_TEST_BM_DEPLOY_DIR") {
        return std::path::PathBuf::from(dir);
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("deploy/bare-metal"))
        .unwrap_or_else(|| std::path::PathBuf::from("deploy/bare-metal"))
}
