// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::cdi::spec::{ContainerEdits, DeviceNode};
use crate::inject::InjectActions;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GresEntry {
    pub name: String,
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub multiple_files: Option<String>,
    #[serde(default)]
    pub count: Option<u64>,
    #[serde(default)]
    pub cores: Option<String>,
    #[serde(default)]
    pub links: Option<String>,
    #[serde(default)]
    pub flags: Vec<String>,
}

impl GresEntry {
    pub fn is_count_only(&self) -> bool {
        self.flags.iter().any(|f| f == "count_only")
            || (self.file.is_none() && self.multiple_files.is_none())
    }

    pub fn has_file_spec(&self) -> bool {
        self.file.is_some() || self.multiple_files.is_some()
    }
}

pub fn expand_hostlist(pattern: &str) -> Vec<String> {
    let Some(bracket_start) = pattern.find('[') else {
        return vec![pattern.to_owned()];
    };
    let Some(bracket_end) = pattern.find(']') else {
        return vec![pattern.to_owned()];
    };

    let prefix = &pattern[..bracket_start];
    let suffix = &pattern[bracket_end + 1..];
    let range_str = &pattern[bracket_start + 1..bracket_end];

    let mut results = Vec::new();
    for part in range_str.split(',') {
        let part = part.trim();
        if let Some((start_s, end_s)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (start_s.parse::<u32>(), end_s.parse::<u32>()) {
                for i in start..=end {
                    results.push(format!("{}{}{}", prefix, i, suffix));
                }
            }
        } else if let Ok(n) = part.parse::<u32>() {
            results.push(format!("{}{}{}", prefix, n, suffix));
        }
    }

    if results.is_empty() {
        vec![pattern.to_owned()]
    } else {
        results
    }
}

pub fn expand_multiple_files(pattern: &str) -> Vec<String> {
    if pattern.contains('[') {
        return expand_hostlist(pattern);
    }

    pattern
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GresFlags {
    pub count_only: bool,
    pub amd_gpu_env: bool,
    pub nvidia_gpu_env: bool,
    pub intel_gpu_env: bool,
    pub opencl_env: bool,
    pub no_gpu_env: bool,
}

impl GresFlags {
    pub(crate) fn from_strings(flags: &[String]) -> Self {
        let mut g = Self::default();
        for flag in flags {
            match flag.as_str() {
                "count_only" => g.count_only = true,
                "amd_gpu_env" => g.amd_gpu_env = true,
                "nvidia_gpu_env" => g.nvidia_gpu_env = true,
                "intel_gpu_env" => g.intel_gpu_env = true,
                "opencl_env" => g.opencl_env = true,
                "no_gpu_env" => {
                    return Self {
                        no_gpu_env: true,
                        ..Default::default()
                    }
                }
                _ => {}
            }
        }
        g
    }

    pub(crate) fn has_env_tokens(&self) -> bool {
        self.amd_gpu_env || self.nvidia_gpu_env || self.intel_gpu_env || self.opencl_env
    }
}

pub(crate) fn build_device_edits(paths: &[String]) -> ContainerEdits {
    let mut device_edits = ContainerEdits::default();
    for path in paths {
        device_edits.device_nodes.push(DeviceNode {
            path: path.clone(),
            host_path: None,
            r#type: None,
            major: None,
            minor: None,
            file_mode: None,
            permissions: Some("rw".into()),
            uid: None,
            gid: None,
        });
    }
    device_edits
}

pub(crate) fn build_shared_edits(paths: &[String]) -> ContainerEdits {
    let mut shared_edits = ContainerEdits::default();
    if paths.iter().any(|p| p.contains("/dev/dri/")) {
        shared_edits.device_nodes.push(DeviceNode {
            path: "/dev/kfd".into(),
            host_path: None,
            r#type: None,
            major: None,
            minor: None,
            file_mode: None,
            permissions: Some("rw".into()),
            uid: None,
            gid: None,
        });
    }
    shared_edits
}

pub(crate) fn inject_actions_for_gres(flags: &GresFlags, paths: &[String]) -> InjectActions {
    InjectActions {
        gpu_visibility_vars: if flags.no_gpu_env {
            HashSet::new()
        } else if flags.has_env_tokens() {
            inject_actions_from_gres_flags(flags).gpu_visibility_vars
        } else {
            infer_gpu_visibility_from_paths(paths)
        },
    }
}

pub(crate) fn inject_actions_from_gres_flags(flags: &GresFlags) -> InjectActions {
    let mut vars = HashSet::new();
    if flags.amd_gpu_env {
        vars.insert("ROCR_VISIBLE_DEVICES".into());
    }
    if flags.nvidia_gpu_env {
        vars.insert("CUDA_VISIBLE_DEVICES".into());
    }
    if flags.intel_gpu_env {
        vars.insert("ZE_AFFINITY_MASK".into());
    }
    if flags.opencl_env {
        vars.insert("GPU_DEVICE_ORDINAL".into());
    }
    InjectActions {
        gpu_visibility_vars: vars,
    }
}

pub(crate) fn infer_gpu_visibility_from_paths(paths: &[String]) -> HashSet<String> {
    for path in paths {
        if path.contains("/dev/dri/") {
            return HashSet::from(["ROCR_VISIBLE_DEVICES".into()]);
        }
        if path.contains("/dev/nvidia") {
            return HashSet::from(["CUDA_VISIBLE_DEVICES".into()]);
        }
    }
    HashSet::new()
}

pub(crate) fn infer_vendor_from_paths(paths: &[String]) -> Option<String> {
    for path in paths {
        if path.contains("/dev/dri/") {
            return Some("amd".into());
        }
        if path.contains("/dev/nvidia") {
            return Some("nvidia".into());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_hostlist() {
        assert_eq!(
            expand_hostlist("/dev/dri/renderD[128-130]"),
            vec![
                "/dev/dri/renderD128",
                "/dev/dri/renderD129",
                "/dev/dri/renderD130"
            ]
        );
        assert_eq!(
            expand_hostlist("/dev/block[0-2]"),
            vec!["/dev/block0", "/dev/block1", "/dev/block2"]
        );
        assert_eq!(expand_hostlist("/dev/kfd"), vec!["/dev/kfd"]);
        assert_eq!(
            expand_hostlist("/dev/dri/renderD[128,130-131]"),
            vec![
                "/dev/dri/renderD128",
                "/dev/dri/renderD130",
                "/dev/dri/renderD131"
            ]
        );
    }

    #[test]
    fn test_expand_multiple_files_comma_split() {
        assert_eq!(
            expand_multiple_files("/dev/dri/renderD128,/dev/dri/card0,/dev/kfd"),
            vec!["/dev/dri/renderD128", "/dev/dri/card0", "/dev/kfd"]
        );
    }

    #[test]
    fn test_expand_multiple_files_bracket() {
        assert_eq!(
            expand_multiple_files("/dev/dri/renderD[128-131]"),
            vec![
                "/dev/dri/renderD128",
                "/dev/dri/renderD129",
                "/dev/dri/renderD130",
                "/dev/dri/renderD131"
            ]
        );
    }

    #[test]
    fn test_gres_flags_to_inject_actions() {
        let f = GresFlags::from_strings(&["amd_gpu_env".into()]);
        let actions = inject_actions_from_gres_flags(&f);
        assert!(actions.gpu_visibility_vars.contains("ROCR_VISIBLE_DEVICES"));
        assert!(!actions.gpu_visibility_vars.contains("CUDA_VISIBLE_DEVICES"));

        let f = GresFlags::from_strings(&["no_gpu_env".into()]);
        let actions = inject_actions_from_gres_flags(&f);
        assert!(actions.gpu_visibility_vars.is_empty());
    }

    #[test]
    fn test_inject_actions_for_gres_path_default() {
        let flags = GresFlags::default();
        let actions = inject_actions_for_gres(&flags, &["/dev/dri/renderD128".into()]);
        assert!(actions.gpu_visibility_vars.contains("ROCR_VISIBLE_DEVICES"));

        let flags = GresFlags::from_strings(&["intel_gpu_env".into()]);
        let actions = inject_actions_for_gres(&flags, &["/dev/dri/renderD128".into()]);
        assert!(actions.gpu_visibility_vars.contains("ZE_AFFINITY_MASK"));
        assert!(!actions.gpu_visibility_vars.contains("ROCR_VISIBLE_DEVICES"));
    }
}
