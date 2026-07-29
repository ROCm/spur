// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = ["../../proto/slurm.proto", "../../proto/raft_internal.proto"];
    // The .proto files live outside this crate's directory, so cargo's default
    // "rerun if a crate file changed" heuristic never watches them. Track them
    // explicitly, otherwise editing a proto leaves the generated code stale.
    for proto in &protos {
        println!("cargo:rerun-if-changed={proto}");
    }
    tonic_prost_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .build_server(true)
        .build_client(true)
        .compile_protos(&protos, &["../../proto"])?;
    Ok(())
}
