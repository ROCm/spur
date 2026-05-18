use std::path::Path;

use anyhow::{bail, Context, Result};
use openssh::{KnownHosts, Session, SessionBuilder, Stdio};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// SSH session to a test node.
pub struct SshNode {
    pub host: String,
    pub user: String,
    ssh_key: Option<std::path::PathBuf>,
    session: Session,
}

impl SshNode {
    pub async fn connect(host: &str, user: &str, ssh_key: Option<&Path>) -> Result<Self> {
        let mut builder = SessionBuilder::default();
        builder.user(user.to_string());
        builder.known_hosts_check(KnownHosts::Accept);
        if let Some(key) = ssh_key {
            builder.keyfile(key);
        }

        let destination = format!("{user}@{host}");
        let session = builder
            .connect(&destination)
            .await
            .with_context(|| format!("SSH connect to {destination}"))?;

        Ok(Self {
            host: host.to_string(),
            user: user.to_string(),
            ssh_key: ssh_key.map(|p| p.to_path_buf()),
            session,
        })
    }

    pub fn destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// Run a remote shell command and return stdout (stderr merged on failure).
    pub async fn exec(&self, cmd: &str) -> Result<String> {
        let output = self
            .session
            .command("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .await
            .with_context(|| format!("exec on {}: {cmd}", self.host))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            bail!(
                "command failed on {} (exit {:?}): {}\nstdout: {}",
                self.host,
                output.status.code(),
                stderr,
                stdout
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Run a remote command, ignoring non-zero exit (returns stdout+stderr text).
    pub async fn exec_allow_fail(&self, cmd: &str) -> Result<String> {
        let output = self
            .session
            .command("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .await
            .with_context(|| format!("exec on {}: {cmd}", self.host))?;

        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        Ok(combined)
    }

    pub async fn kill_processes(&self, pattern: &str) -> Result<()> {
        let _ = self
            .exec_allow_fail(&format!("pkill -f '{pattern}' 2>/dev/null || true"))
            .await;
        Ok(())
    }

    pub async fn mkdir_p(&self, path: &str) -> Result<()> {
        self.exec(&format!("mkdir -p '{path}'")).await?;
        Ok(())
    }

    /// Create a unique directory with `mktemp -d` (e.g. `/tmp/spur-ci.XXXXXX`).
    pub async fn mktemp_dir(&self, template: &str) -> Result<String> {
        // Non-interactive SSH often has a minimal PATH; use absolute path to mktemp.
        let path = self
            .exec(&format!("/usr/bin/mktemp -d '{template}'"))
            .await
            .with_context(|| format!("mktemp on {}", self.host))?;
        let path = path.trim().to_string();
        if path.is_empty() {
            bail!("mktemp returned empty path on {}", self.host);
        }
        Ok(path)
    }

    pub async fn rm_rf(&self, path: &str) -> Result<()> {
        let _ = self
            .exec_allow_fail(&format!("rm -rf '{path}' 2>/dev/null || true"))
            .await;
        Ok(())
    }

    pub async fn write_remote_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        let mut child = self
            .session
            .command("sh")
            .arg("-c")
            .arg(format!("cat > '{path}'"))
            .stdin(Stdio::piped())
            .spawn()
            .await
            .with_context(|| format!("write remote file {path} on {}", self.host))?;

        if let Some(mut stdin) = child.stdin().take() {
            stdin.write_all(contents).await?;
            stdin.shutdown().await?;
        }

        let status = child.wait().await?;
        if !status.success() {
            bail!("failed to write {path} on {}", self.host);
        }
        Ok(())
    }

    pub async fn read_remote_file(&self, path: &str) -> Result<String> {
        self.exec(&format!("test -f '{path}' && cat '{path}' || echo ''"))
            .await
    }

    pub async fn scp_to(&self, local: &Path, remote: &str) -> Result<()> {
        let mut cmd = Command::new("scp");
        cmd.arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg("BatchMode=yes");
        if let Some(key) = &self.ssh_key {
            cmd.arg("-i").arg(key);
        }
        cmd.arg(local)
            .arg(format!("{}:{}", self.destination(), remote));
        let output = cmd
            .output()
            .await
            .with_context(|| format!("scp {} -> {}:{}", local.display(), self.host, remote))?;
        if !output.status.success() {
            bail!("scp failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    }

    pub async fn scp_from(&self, remote: &str, local: &Path) -> Result<()> {
        let mut cmd = Command::new("scp");
        cmd.arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg("BatchMode=yes");
        if let Some(key) = &self.ssh_key {
            cmd.arg("-i").arg(key);
        }
        cmd.arg(format!("{}:{}", self.destination(), remote))
            .arg(local);
        let output = cmd
            .output()
            .await
            .with_context(|| format!("scp {}:{} -> {}", self.host, remote, local.display()))?;
        if !output.status.success() {
            bail!("scp failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    }
}
