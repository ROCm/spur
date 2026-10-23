// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod device_registry;
pub mod entry;

pub use device_registry::DeviceRegistry;
pub use entry::{
    entry_matches_gres_name, kind_matches_gres_name, kind_to_gres_name, resolve_link_type,
    DeviceCapability, DeviceEntry,
};
