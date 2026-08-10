// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// SPANK C/`slurm_*` API symbols (defined in spur-spank) that dlopen'd plugins
/// resolve against this executable. They have no direct caller in spurd.
const SPANK_SYMBOLS: &[&str] = &[
    "spank_get_item",
    "spank_setenv",
    "spank_getenv",
    "spank_unsetenv",
    "spank_job_control_setenv",
    "spank_job_control_getenv",
    "spank_job_control_unsetenv",
    "spank_strerror",
    "slurm_error",
    "slurm_info",
    "slurm_verbose",
    "slurm_debug",
    "slurm_debug2",
    "slurm_debug3",
    "slurm_spank_log",
];

fn main() {
    // GNU ld / lld only; the flags below are meaningless elsewhere. Guard on the
    // target (not host) OS so cross-compiles behave correctly.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    // --undefined forces the linker to pull each symbol from the spur-spank rlib
    // despite having no caller.
    for sym in SPANK_SYMBOLS {
        println!("cargo::rustc-link-arg-bins=-Wl,--undefined={sym}");
    }

    // Export ONLY these symbols into the dynamic symbol table so plugin dlsym
    // resolves them. A scoped --dynamic-list avoids --export-dynamic exposing
    // every global in the binary.
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let list_path = std::path::Path::new(&out_dir).join("spank_dynamic_list.ld");
    let mut list = String::from("{\n");
    for sym in SPANK_SYMBOLS {
        list.push_str(&format!("    {sym};\n"));
    }
    list.push_str("};\n");
    std::fs::write(&list_path, list).expect("write dynamic list");
    println!("cargo::rerun-if-changed=build.rs");
    println!(
        "cargo::rustc-link-arg-bins=-Wl,--dynamic-list={}",
        list_path.display()
    );
}
