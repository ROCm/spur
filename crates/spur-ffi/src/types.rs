// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! #[repr(C)] types matching Slurm's ABI layout.

use std::os::raw::{c_char, c_uint};

/// Job submission descriptor (simplified).
#[repr(C)]
pub struct SlurmJobDescMsg {
    pub name: *const c_char,
    pub partition: *const c_char,
    pub account: *const c_char,
    pub script: *const c_char,
    pub work_dir: *const c_char,
    pub min_nodes: c_uint,
    pub max_nodes: c_uint,
    pub num_tasks: c_uint,
    pub cpus_per_task: c_uint,
    pub time_limit: c_uint, // minutes
    pub priority: c_uint,
}

impl Default for SlurmJobDescMsg {
    fn default() -> Self {
        Self {
            name: std::ptr::null(),
            partition: std::ptr::null(),
            account: std::ptr::null(),
            script: std::ptr::null(),
            work_dir: std::ptr::null(),
            min_nodes: 1,
            max_nodes: 1,
            num_tasks: 1,
            cpus_per_task: 1,
            time_limit: 0,
            priority: 0,
        }
    }
}

/// Job info record (simplified).
#[repr(C)]
pub struct SlurmJobInfo {
    pub job_id: c_uint,
    pub name: *mut c_char,
    pub user_name: *mut c_char,
    pub partition: *mut c_char,
    pub account: *mut c_char,
    pub job_state: c_uint,
    pub num_nodes: c_uint,
    pub num_tasks: c_uint,
    pub exit_code: i32,
    pub nodelist: *mut c_char,
}

/// Job info message (container for multiple job records).
pub struct SlurmJobInfoMsg {
    pub record_count: c_uint,
    pub job_array: *mut SlurmJobInfo,
    /// Rust-owned storage to keep the Vec alive.
    pub _job_storage: Vec<SlurmJobInfo>,
}

/// Node info record (simplified).
#[repr(C)]
pub struct SlurmNodeInfo {
    pub name: *mut c_char,
    pub node_state: c_uint,
    pub cpus: c_uint,
    pub real_memory: u64,
    pub reason: *mut c_char,
}

/// Node info message.
pub struct SlurmNodeInfoMsg {
    pub record_count: c_uint,
    pub node_array: *mut SlurmNodeInfo,
    pub _node_storage: Vec<SlurmNodeInfo>,
}

/// Partition info record (simplified).
#[repr(C)]
pub struct SlurmPartInfo {
    pub name: *mut c_char,
    pub total_nodes: c_uint,
    pub total_cpus: c_uint,
    pub nodes: *mut c_char,
}

/// Partition info message.
pub struct SlurmPartInfoMsg {
    pub record_count: c_uint,
    pub partition_array: *mut SlurmPartInfo,
    pub _part_storage: Vec<SlurmPartInfo>,
}
