// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod annotations;
pub mod cache;
pub mod discovery;
pub mod spec;

pub use cache::{CacheErrors, CachedDevice, CdiCache, DEFAULT_SPEC_DIRS};
pub use discovery::discover_to_cdi;
pub use spec::{
    parse_qualified_name, CdiDevice, CdiSpec, ContainerEdits, DeviceNode, QualifiedName,
    GENERATED_CDI_VERSION,
};
