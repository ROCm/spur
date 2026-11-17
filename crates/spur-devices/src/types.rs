// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    Pcie,
    Xgmi,
    Nvlink,
}

/// Allocated devices of a job by gres name ("gpu", "nic", ...), as stable ids.
pub type DeviceSelection = std::collections::BTreeMap<String, Vec<u64>>;
