// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Live device discovery: each vendor/kind module reads sysfs and emits CDI specs.

mod amd_gpu;
mod amd_nic;

use std::path::Path;

use crate::cdi::spec::CdiSpec;

pub use amd_gpu::discover_gpu_cdi;
pub use amd_nic::{NicEvent, NicFunction, NicLog, NicOutcome, NicScan, Skipped};

/// One host scan: a CDI spec per kind present, and what became of every NIC.
pub struct Discovery {
    pub specs: Vec<CdiSpec>,
    pub nics: NicScan,
}

pub fn discover_all_cdi() -> Discovery {
    discover_all_cdi_from_root(Path::new("/"))
}

pub(crate) fn discover_all_cdi_from_root(root: &Path) -> Discovery {
    let mut specs = amd_gpu::discover_gpu_cdi_from_root(root);
    let nics = amd_nic::discover_nic_cdi_from_root(root);
    specs.extend(nics.specs);
    Discovery {
        specs,
        nics: nics.scan,
    }
}
