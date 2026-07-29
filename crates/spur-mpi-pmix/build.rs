// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

/// Profile output dir derived from `OUT_DIR` so it honors `--target-dir`
/// (e.g. `cargo llvm-cov`) instead of a guessed path that need not exist.
fn profile_target_dir(out_dir: &Path) -> Option<PathBuf> {
    let mut dir = out_dir;
    while let Some(parent) = dir.parent() {
        if parent.file_name().is_some_and(|n| n == "build") {
            return parent.parent().map(Path::to_path_buf);
        }
        dir = parent;
    }
    None
}

fn pkg_config_link_other(lib: &str) -> Vec<String> {
    std::process::Command::new("pkg-config")
        .args(["--libs-only-other", lib])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn copy_plugin(
    out_dir: &Path,
    archive_name: &str,
    pmix_libs: &[String],
    pmix_link_paths: &[PathBuf],
    pmix_link_other: &[String],
) {
    let archive = out_dir.join(format!("lib{archive_name}.a"));
    let built = out_dir.join("libspur_mpi_pmix.so");

    let mut cmd = cc::Build::new().get_compiler().to_command();
    cmd.arg("-shared").arg("-o").arg(&built).arg(format!(
        "-Wl,--whole-archive,{},--no-whole-archive",
        archive.display()
    ));
    for link_path in pmix_link_paths {
        cmd.arg(format!("-L{}", link_path.display()));
    }
    for arg in pmix_link_other {
        cmd.arg(arg);
    }
    for lib in pmix_libs {
        cmd.arg(format!("-l{lib}"));
    }
    cmd.arg("-pthread");
    if !cmd.status().expect("link spur_mpi_pmix.so").success() {
        panic!("failed to link {built:?}");
    }

    // Release-only, best-effort: place the plugin next to the release binaries
    // for packaging/e2e without dropping uninstrumented .so where llvm-cov scans.
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let Some(target_dir) = profile_target_dir(out_dir) else {
        return;
    };
    for name in ["libspur_mpi_pmix.so", "spur_mpi_pmix.so"] {
        if let Err(e) = std::fs::copy(&built, target_dir.join(name)) {
            println!("cargo:warning=failed to stage {name} into {target_dir:?}: {e}");
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=c/pmix_server.c");
    println!("cargo:rerun-if-changed=c/stub_server.c");
    println!("cargo:rerun-if-changed=include/spur_mpi_plugin.h");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

    let mut build = cc::Build::new();
    build.pic(true);
    build.include("include");
    build.cargo_metadata(false);

    if let Ok(pmix) = pkg_config::Config::new().probe("pmix") {
        let link_other = pkg_config_link_other("pmix");
        for include in &pmix.include_paths {
            build.include(include);
        }
        build.file("c/pmix_server.c");
        build.compile("spur_mpi_pmix_server");
        copy_plugin(
            &out_dir,
            "spur_mpi_pmix_server",
            &pmix.libs,
            &pmix.link_paths,
            &link_other,
        );
    } else {
        build.file("c/stub_server.c");
        build.compile("spur_mpi_pmix_stub");
        copy_plugin(&out_dir, "spur_mpi_pmix_stub", &[], &[], &[]);
    }
}
