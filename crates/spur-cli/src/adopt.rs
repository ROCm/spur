// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur adopt` — pam_exec helper that adopts a compute-node SSH session into
//! the allocation its user holds on the node.
//!
//! Wire it into sshd's PAM stack, e.g.:
//!
//! ```text
//! account  optional  pam_exec.so          /usr/bin/spur adopt
//! session  optional  pam_exec.so seteuid  /usr/bin/spur adopt
//! ```
//!
//! pam_exec runs this once per phase and passes `PAM_USER` / `PAM_TYPE` in the
//! environment. Behaviour by phase:
//!
//! - `account` — admission. When `SPUR_REQUIRE_ALLOCATION=1` is set on the
//!   pam line, refuse (exit non-zero) a user who holds no allocation on this
//!   node; otherwise always permit. Off by default so unadopted sessions keep
//!   working where site policy allows them.
//! - `open_session` — adoption. Join the session's process to the allocation's
//!   cgroup so it is under the job's resource control and is swept when the
//!   allocation ends (#799). The job environment is written to stdout as
//!   `export` lines for a wrapper to apply (pam_exec cannot set session env
//!   itself).
//!
//! Answered by the local agent's `AdoptSession` RPC. Best-effort: any error in
//! the session phase is logged and ignored so a transient fault never locks a
//! user out of a node.

use anyhow::{Context, Result};
use spur_proto::proto::AdoptSessionRequest;

/// Where the local node agent listens; override for testing.
fn agent_addr() -> String {
    std::env::var("SPUR_AGENT_ADDR").unwrap_or_else(|_| "http://127.0.0.1:6818".to_string())
}

fn uid_for_user(user: &str) -> Option<u32> {
    // Numeric PAM_USER, or resolve the name via NSS.
    if let Ok(uid) = user.parse::<u32>() {
        return Some(uid);
    }
    let cuser = std::ffi::CString::new(user).ok()?;
    let pw = unsafe { libc::getpwnam(cuser.as_ptr()) };
    if pw.is_null() {
        None
    } else {
        Some(unsafe { (*pw).pw_uid })
    }
}

pub async fn main_with_args(_args: Vec<String>) -> Result<()> {
    let pam_type = std::env::var("PAM_TYPE").unwrap_or_default();
    let pam_user = std::env::var("PAM_USER").unwrap_or_default();

    // Only the account (admission) and open_session (adoption) phases act.
    if pam_type != "account" && pam_type != "open_session" {
        return Ok(());
    }

    let Some(uid) = uid_for_user(&pam_user) else {
        // Cannot resolve the user: deny admission (fail closed), no-op adoption.
        if pam_type == "account" && require_allocation() {
            std::process::exit(1);
        }
        return Ok(());
    };

    let resp = match query_allocation(uid).await {
        Ok(r) => r,
        Err(e) => {
            // Fail open on adoption; fail closed on admission only if required.
            eprintln!("spur adopt: agent query failed: {e:#}");
            if pam_type == "account" && require_allocation() {
                std::process::exit(1);
            }
            return Ok(());
        }
    };

    match pam_type.as_str() {
        "account" if require_allocation() && !resp.has_allocation => {
            eprintln!("spur adopt: {pam_user} holds no allocation on this node; access refused");
            std::process::exit(1);
        }
        "open_session" if resp.has_allocation => adopt_into(&resp),
        _ => {}
    }
    Ok(())
}

fn require_allocation() -> bool {
    matches!(
        std::env::var("SPUR_REQUIRE_ALLOCATION").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

async fn query_allocation(uid: u32) -> Result<spur_proto::proto::AdoptSessionResponse> {
    let mut agent = crate::interactive::connect_agent(&agent_addr())
        .await
        .context("connect to local agent")?;
    let resp = agent
        .adopt_session(AdoptSessionRequest { uid })
        .await
        .context("AdoptSession RPC")?
        .into_inner();
    Ok(resp)
}

/// Join the session into the job cgroup and emit the job environment.
fn adopt_into(resp: &spur_proto::proto::AdoptSessionResponse) {
    // Move the session's process (pam_exec's parent — the sshd session leader)
    // into the job cgroup; its shell inherits the membership.
    if !resp.cgroup_path.is_empty() {
        let procs = std::path::Path::new(&resp.cgroup_path).join("cgroup.procs");
        let ppid = unsafe { libc::getppid() };
        if let Err(e) = std::fs::write(&procs, ppid.to_string()) {
            eprintln!("spur adopt: could not join cgroup {}: {e}", procs.display());
        }
    }

    // pam_exec does not propagate our environment to the session, so emit the
    // job env as shell `export` lines for a wrapper (or a sourced rc) to apply.
    let mut keys: Vec<&String> = resp.environment.keys().collect();
    keys.sort();
    for k in keys {
        if let Some(v) = resp.environment.get(k) {
            println!("export {}={}", k, shell_quote(v));
        }
    }
}

fn shell_quote(v: &str) -> String {
    if v.is_empty() {
        return "''".to_string();
    }
    if v.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b','))
    {
        v.to_string()
    } else {
        format!("'{}'", v.replace('\'', r"'\''"))
    }
}
