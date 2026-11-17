// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod cdi;
pub mod discovery;
pub mod gres;
pub mod inject;
pub mod pci;
pub mod registry;
pub mod selection;
mod sysfs;
pub mod topology;
pub mod types;
pub mod ualink;

pub use cdi::annotations::DeviceMetadata;
pub use gres::{expand_hostlist, expand_multiple_files, GresCache, GresEntry};
pub use inject::{
    build_injection_plans, merge_edits_from_devices, ContainerInjectionPlan, HostInjectionPlan,
    InjectActions, MergedEdits,
};
pub use registry::{DeviceCapability, DeviceClass, DeviceEntry, DeviceRegistry};
pub use selection::DeviceSelection;
pub use types::LinkType;
