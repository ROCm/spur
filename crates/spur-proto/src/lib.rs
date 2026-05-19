// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod proto {
    tonic::include_proto!("slurm");
}

pub mod raft_proto {
    tonic::include_proto!("raft_internal");
}

pub use proto::*;
