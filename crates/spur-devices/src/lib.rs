// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod cdi;
pub mod gres;
pub mod inject;
pub mod registry;
pub mod types;

pub use cdi::annotations::DeviceMetadata;
pub use gres::{expand_hostlist, expand_multiple_files, GresCache, GresEntry};
pub use inject::{
    build_injection_plans, merge_edits_from_devices, ContainerInjectionPlan, HostInjectionPlan,
    InjectActions, MergedEdits,
};
pub use registry::{resolve_link_type, DeviceCapability, DeviceEntry, DeviceRegistry};
pub use types::LinkType;
