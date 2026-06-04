// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &["../../proto/slurm.proto", "../../proto/raft_internal.proto"],
            &["../../proto"],
        )?;
    Ok(())
}
