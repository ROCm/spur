// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

fn main() {
    // SPANK plugins are dlopen'd at runtime and resolve the spank_*/slurm_*
    // API symbols (defined in spur-spank) against this executable. Rust does
    // not place #[no_mangle] symbols in the dynamic symbol table by default,
    // so export them explicitly. GNU ld / lld only.
    if cfg!(target_os = "linux") {
        println!("cargo::rustc-link-arg-bins=-Wl,--export-dynamic");
    }
}
