// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Client-side pre-check for privileged CLI actions.
//!
//! Kept for the error it gives — immediate, offline, naming the action — but enforcement is the
//! controller's (`require_reservation_manager`). Checks the invoking process, which no server can
//! verify, and does not prove the caller elevated via `sudo`. The rule lives in
//! [`spur_core::privilege`] so the two sides cannot drift.

use anyhow::{bail, Context, Result};
use spur_core::privilege::{is_privileged, PRIVILEGED_GROUPS, PRIVILEGE_REQUIREMENT};

/// Gate a privileged CLI action. Fails closed: any identity/group lookup error denies the action
/// rather than allowing it.
pub fn require_privileged(action: &str) -> Result<()> {
    let euid = nix::unistd::geteuid().as_raw();
    // Root is always privileged; short-circuit so a group-lookup failure can never deny root.
    if euid == 0 {
        return Ok(());
    }
    let groups = spur_core::privilege::current_process_groups()
        .context("failed to read the invoking process's groups")?;
    if is_privileged(euid, &groups, PRIVILEGED_GROUPS) {
        return Ok(());
    }
    bail!("insufficient privileges to {action}: {PRIVILEGE_REQUIREMENT}");
}
