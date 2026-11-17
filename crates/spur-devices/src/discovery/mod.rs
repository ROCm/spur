// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Live device discovery: each vendor/kind module reads sysfs and emits CDI specs.

mod amd_gpu;
mod amd_nic;

use std::path::Path;

use crate::cdi::spec::CdiSpec;

pub use amd_gpu::discover_gpu_cdi;

/// Every device kind discovered on this host, one CDI spec per kind present.
pub fn discover_all_cdi() -> Vec<CdiSpec> {
    discover_all_cdi_from_root(Path::new("/"))
}

pub(crate) fn discover_all_cdi_from_root(root: &Path) -> Vec<CdiSpec> {
    let mut specs = amd_gpu::discover_gpu_cdi_from_root(root);
    specs.extend(amd_nic::discover_nic_cdi_from_root(root));
    specs
}
