// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use tracing::warn;

use crate::cdi::annotations;
use crate::cdi::spec::ContainerEdits;
use crate::gres::entry::{
    build_device_edits, build_shared_edits, expand_hostlist, expand_multiple_files,
    infer_vendor_from_paths, inject_actions_for_gres, GresEntry, GresFlags,
};
use crate::inject::InjectActions;
use crate::registry::entry::collect_device_paths;

#[derive(Clone, Debug, Default)]
pub struct GresCache {
    pub injectable: Vec<ExpandedGresDevice>,
    pub countable: Vec<CountableGresPool>,
    pub source_entries: Vec<GresEntry>,
}

#[derive(Clone, Debug)]
pub struct ExpandedGresDevice {
    pub gres_name: String,
    pub resource_type: Option<String>,
    pub vendor: Option<String>,
    pub inject_actions: InjectActions,
    pub cores: Option<Vec<u32>>,
    pub links: Option<Vec<i32>>,
    pub device_edits: ContainerEdits,
    pub shared_edits: ContainerEdits,
    pub device_paths: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CountableGresPool {
    pub gres_name: String,
    pub resource_type: Option<String>,
    pub capacity: u64,
}

impl GresCache {
    pub fn from_entries(entries: &[GresEntry]) -> Self {
        let mut cache = Self {
            source_entries: entries.to_vec(),
            ..Default::default()
        };

        for gres in entries {
            let gres_flags = GresFlags::from_strings(&gres.flags);

            if gres.is_count_only() || gres_flags.count_only {
                cache.countable.push(CountableGresPool {
                    gres_name: gres.name.clone(),
                    resource_type: gres.r#type.clone(),
                    capacity: gres.count.unwrap_or(0),
                });
                continue;
            }

            for device in expand_gres_injectables(gres) {
                cache.injectable.push(device);
            }
        }

        cache
    }
}

fn expand_gres_injectables(gres: &GresEntry) -> Vec<ExpandedGresDevice> {
    if gres.file.is_some() && gres.multiple_files.is_some() {
        warn!(
            gres_name = %gres.name,
            "GRES entry has both file and multiple_files; using multiple_files"
        );
    }

    let path_groups: Vec<Vec<String>> = if let Some(ref multiple_files) = gres.multiple_files {
        let paths = expand_multiple_files(multiple_files);
        if paths.is_empty() {
            return Vec::new();
        }
        vec![paths]
    } else if let Some(ref file) = gres.file {
        expand_hostlist(file)
            .into_iter()
            .map(|path| vec![path])
            .collect()
    } else {
        return Vec::new();
    };

    path_groups
        .iter()
        .filter_map(|paths| build_expanded_device(gres, paths))
        .collect()
}

fn build_expanded_device(gres: &GresEntry, paths: &[String]) -> Option<ExpandedGresDevice> {
    if paths.is_empty() {
        return None;
    }

    let gres_flags = GresFlags::from_strings(&gres.flags);
    let device_edits = build_device_edits(paths);
    let shared_edits = build_shared_edits(paths);
    let device_paths = collect_device_paths(&device_edits, &shared_edits);

    Some(ExpandedGresDevice {
        gres_name: gres.name.clone(),
        resource_type: gres.r#type.clone(),
        vendor: infer_vendor_from_paths(paths),
        inject_actions: inject_actions_for_gres(&gres_flags, paths),
        cores: gres
            .cores
            .as_ref()
            .and_then(|s| annotations::parse_cores(s)),
        links: gres
            .links
            .as_ref()
            .and_then(|s| annotations::parse_links(s)),
        device_edits,
        shared_edits,
        device_paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_entries_file_mode() {
        let cache = GresCache::from_entries(&[GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD[128-129]".into()),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }]);

        assert_eq!(cache.injectable.len(), 2);
        assert!(cache.injectable[0]
            .inject_actions
            .gpu_visibility_vars
            .contains("ROCR_VISIBLE_DEVICES"));
        assert!(cache.injectable[0]
            .shared_edits
            .device_nodes
            .iter()
            .any(|n| n.path == "/dev/kfd"));
    }

    #[test]
    fn test_from_entries_count_only() {
        let cache = GresCache::from_entries(&[GresEntry {
            name: "bandwidth".into(),
            r#type: Some("lustre".into()),
            count: Some(4096),
            flags: vec!["count_only".into()],
            ..Default::default()
        }]);

        assert_eq!(cache.countable.len(), 1);
        assert_eq!(cache.countable[0].capacity, 4096);
        assert!(cache.injectable.is_empty());
    }

    #[test]
    fn test_from_entries_multiple_files() {
        let cache = GresCache::from_entries(&[GresEntry {
            name: "gpu".into(),
            r#type: Some("mi300x".into()),
            multiple_files: Some("/dev/dri/renderD128,/dev/dri/card0".into()),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }]);

        assert_eq!(cache.injectable.len(), 1);
        assert_eq!(cache.injectable[0].device_edits.device_nodes.len(), 2);
        assert_eq!(cache.injectable[0].device_paths.len(), 3);
        assert!(cache.injectable[0]
            .device_paths
            .contains(&"/dev/kfd".to_string()));
    }

    #[test]
    fn test_from_entries_multiple_files_bracket() {
        let cache = GresCache::from_entries(&[GresEntry {
            name: "gpu".into(),
            multiple_files: Some("/dev/dri/renderD[128-131]".into()),
            ..Default::default()
        }]);

        assert_eq!(cache.injectable.len(), 1);
        assert_eq!(cache.injectable[0].device_edits.device_nodes.len(), 4);
    }
}
