// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! sbcast fan-out target: write a broadcast file to this node's local storage.
//!
//! The controller supplies the job owner's uid/gid/work_dir, so this module is
//! stateless about the job. When spurd runs as root and the job user is not
//! root, the write is performed *as the job user* via a `setpriv`-dropped child
//! so the kernel enforces where the user may write — a direct root write would
//! let a user overwrite root-owned paths (e.g. /etc/cron.d), a privilege
//! escalation. Otherwise the file is written directly with spurd's own creds.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::privdrop::PrivDrop;

/// Resolve the destination path; a relative path resolves against the job's
/// working directory (Slurm sbcast semantics).
pub(crate) fn resolve_dest(dest: &str, work_dir: &str) -> Result<PathBuf, String> {
    let p = Path::new(dest);
    if p.as_os_str().is_empty() {
        return Err("empty destination path".into());
    }
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else if !work_dir.is_empty() {
        Ok(Path::new(work_dir).join(p))
    } else {
        Err(format!(
            "relative destination '{dest}' but the job has no working directory"
        ))
    }
}

/// Write `data` to `dest` for a job, honoring `force`, `mode`, and the job
/// user's identity.
pub(crate) fn write_file(
    dest: &Path,
    data: &[u8],
    mode: u32,
    force: bool,
    uid: u32,
    gid: u32,
) -> Result<(), String> {
    if !force && dest.exists() {
        return Err(format!(
            "{} exists (use --force to overwrite)",
            dest.display()
        ));
    }
    let mode = if mode == 0 { 0o644 } else { mode & 0o7777 };

    match PrivDrop::resolve_if_needed(uid, gid) {
        Some(pd) => write_as_user(&pd, dest, data, mode),
        None => write_direct(dest, data, mode),
    }
}

fn write_direct(dest: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    let mut f =
        std::fs::File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    f.write_all(data)
        .map_err(|e| format!("write {}: {e}", dest.display()))?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", dest.display()))?;
    Ok(())
}

/// Write via a `setpriv`-dropped `sh` child so the file is created with the job
/// user's credentials.
///
/// ponytail: the redirect is not atomic (no temp+rename) and the `!force` check
/// happens in the caller (a small TOCTOU window) — acceptable for staging a
/// file, upgrade path is an O_NOFOLLOW temp-file + rename done in the child.
fn write_as_user(pd: &PrivDrop, dest: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    let dest_s = dest
        .to_str()
        .ok_or_else(|| "destination path is not valid UTF-8".to_string())?;
    let mut prefix = pd.setpriv_prefix(); // ["setpriv", "--reuid=..", "--regid=..", "--init-groups", "--"]
    let program = prefix.remove(0);

    let mut child = std::process::Command::new(program)
        .args(prefix)
        .arg("/bin/sh")
        .arg("-c")
        .arg(r#"umask 0; cat > "$1" && chmod "$2" "$1""#)
        .arg("sh") // $0
        .arg(dest_s) // $1
        .arg(format!("{mode:o}")) // $2
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn privilege-dropped writer: {e}"))?;

    child
        .stdin
        .take()
        .ok_or_else(|| "writer child has no stdin".to_string())?
        .write_all(data)
        .map_err(|e| format!("feed sbcast data to writer: {e}"))?;

    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait for writer: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "write as job user failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "spur-sbcast-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn resolve_dest_absolute_is_used_verbatim() {
        assert_eq!(
            resolve_dest("/tmp/x", "/home/u").unwrap(),
            PathBuf::from("/tmp/x")
        );
    }

    #[test]
    fn resolve_dest_relative_joins_work_dir() {
        assert_eq!(
            resolve_dest("data/x", "/home/u").unwrap(),
            PathBuf::from("/home/u/data/x")
        );
    }

    #[test]
    fn resolve_dest_relative_without_work_dir_errors() {
        assert!(resolve_dest("x", "").is_err());
        assert!(resolve_dest("", "/home/u").is_err());
    }

    #[test]
    fn write_direct_writes_content_and_mode() {
        // Off-root test runner: uid == euid, so resolve_if_needed returns None
        // and write_file takes the direct path.
        let dir = scratch_dir();
        let dest = dir.join("payload.bin");
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();

        write_file(&dest, b"hello sbcast", 0o640, false, uid, gid).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello sbcast");
        let got = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o7777;
        assert_eq!(got, 0o640, "mode should match the requested bits");

        // Without --force, an existing destination is refused.
        assert!(write_file(&dest, b"again", 0o640, false, uid, gid).is_err());
        // With --force, it overwrites.
        write_file(&dest, b"again", 0o600, true, uid, gid).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"again");

        std::fs::remove_dir_all(&dir).ok();
    }
}
