// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Write};

fn main() -> anyhow::Result<()> {
    let specs = spur_devices::discovery::discover_all_cdi();
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &specs)?;
    writeln!(out)?;
    Ok(())
}
