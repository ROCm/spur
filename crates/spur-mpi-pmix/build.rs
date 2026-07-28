// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

fn main() {
    println!("cargo:rerun-if-changed=c/pmix_server.c");
    println!("cargo:rerun-if-changed=c/stub_server.c");
    println!("cargo:rerun-if-changed=include/spur_mpi_plugin.h");

    let include = std::path::Path::new("include");
    let mut build = cc::Build::new();
    build.include(include);

    if pkg_config::Config::new().probe("pmix").is_ok() {
        build.file("c/pmix_server.c");
        build.compile("spur_mpi_pmix_server");
        println!("cargo:rustc-cfg=spur_pmix_linked");
    } else {
        build.file("c/stub_server.c");
        build.compile("spur_mpi_pmix_stub");
    }
}
