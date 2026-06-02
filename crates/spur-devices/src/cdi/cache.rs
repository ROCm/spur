// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::cdi::spec::{CdiDevice, CdiSpec};

pub const DEFAULT_SPEC_DIRS: &[&str] = &["/etc/cdi", "/var/run/cdi"];

#[derive(Clone, Debug)]
pub struct CachedDevice {
    pub qualified_name: String,
    pub device: CdiDevice,
    pub kind: String,
    pub shared_edits: Option<crate::cdi::spec::ContainerEdits>,
    pub spec_annotations: HashMap<String, String>,
    pub source_path: String,
    pub priority: i32,
}

#[derive(Clone, Debug, Default)]
pub struct CacheErrors {
    pub errors: HashMap<String, Vec<String>>,
}

impl CacheErrors {
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn add(&mut self, path: &str, err: String) {
        self.errors.entry(path.to_owned()).or_default().push(err);
    }
}

impl std::fmt::Display for CacheErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (path, errs) in &self.errors {
            for err in errs {
                writeln!(f, "{}: {}", path, err)?;
            }
        }
        Ok(())
    }
}

pub struct CdiCache {
    devices: HashMap<String, CachedDevice>,
    errors: CacheErrors,
}

impl CdiCache {
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
            errors: CacheErrors::default(),
        }
    }

    pub fn load(cdi_spec_dirs: &[String], auto_detect: bool) -> Self {
        let mut cdi_dirs: Vec<PathBuf> = DEFAULT_SPEC_DIRS.iter().map(PathBuf::from).collect();
        for extra in cdi_spec_dirs {
            cdi_dirs.push(PathBuf::from(extra));
        }

        let mut cache = Self::new();
        let errors = cache.load_from_dirs(&cdi_dirs);
        if !errors.is_empty() {
            warn!("CDI spec loading errors:\n{}", errors);
        }

        if cache.is_empty() && auto_detect {
            cache.add_specs(&crate::cdi::discovery::discover_to_cdi());
        } else if auto_detect {
            warn!("on-disk CDI specs found; auto_detect ignored");
        }

        info!(
            cached_devices = cache.len(),
            vendors = ?cache.list_vendors(),
            "CDI cache initialized"
        );

        cache
    }

    pub fn add_specs(&mut self, specs: &[CdiSpec]) {
        for spec in specs {
            self.index_spec(spec, "", 0);
        }
    }

    pub fn load_from_dirs(&mut self, dirs: &[PathBuf]) -> &CacheErrors {
        self.errors = CacheErrors::default();

        for (i, dir) in dirs.iter().enumerate() {
            let priority = (i as i32) + 1;
            let resolved = match dir.canonicalize() {
                Ok(p) => p,
                Err(_) => {
                    debug!(dir = %dir.display(), "CDI spec directory not found, skipping");
                    continue;
                }
            };
            self.load_dir(&resolved, priority);
        }

        &self.errors
    }

    pub fn get_device(&self, qualified_name: &str) -> Option<&CachedDevice> {
        self.devices.get(qualified_name)
    }

    pub fn list_devices(&self) -> Vec<String> {
        let mut names: Vec<String> = self.devices.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn list_vendors(&self) -> Vec<String> {
        let mut vendors: Vec<String> = self
            .devices
            .values()
            .filter_map(|d| d.kind.split_once('/').map(|(v, _)| v.to_owned()))
            .collect();
        vendors.sort();
        vendors.dedup();
        vendors
    }

    pub fn list_kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = self.devices.values().map(|d| d.kind.clone()).collect();
        kinds.sort();
        kinds.dedup();
        kinds
    }

    pub fn get_errors(&self) -> &CacheErrors {
        &self.errors
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &CachedDevice)> {
        self.devices.iter()
    }

    fn load_dir(&mut self, dir: &Path, priority: i32) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                debug!(dir = %dir.display(), error = %e, "cannot read CDI spec directory");
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !is_cdi_spec_file(&path) {
                continue;
            }

            match CdiSpec::read_file(&path) {
                Ok(spec) => {
                    if let Err(e) = spec.validate() {
                        self.errors
                            .add(&path.display().to_string(), format!("validation: {}", e));
                        warn!(path = %path.display(), error = %e, "invalid CDI spec, skipping");
                        continue;
                    }
                    let path_str = path.display().to_string();
                    self.index_spec(&spec, &path_str, priority);
                    debug!(
                        path = %path.display(),
                        kind = %spec.kind,
                        devices = spec.devices.len(),
                        "loaded CDI spec"
                    );
                }
                Err(e) => {
                    self.errors
                        .add(&path.display().to_string(), format!("parse: {}", e));
                    warn!(path = %path.display(), error = %e, "failed to parse CDI spec");
                }
            }
        }
    }

    fn index_spec(&mut self, spec: &CdiSpec, source_path: &str, priority: i32) {
        for device in &spec.devices {
            let qname = spec.qualified_name(&device.name);

            if let Some(existing) = self.devices.get(&qname) {
                if existing.priority > priority {
                    continue;
                }
            }

            self.devices.insert(
                qname.clone(),
                CachedDevice {
                    qualified_name: qname,
                    device: device.clone(),
                    kind: spec.kind.clone(),
                    shared_edits: spec.container_edits.clone(),
                    spec_annotations: spec.annotations.clone(),
                    source_path: source_path.to_owned(),
                    priority,
                },
            );
        }
    }
}

impl Default for CdiCache {
    fn default() -> Self {
        Self::new()
    }
}

fn is_cdi_spec_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| matches!(ext, "json" | "yaml" | "yml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdi::spec::{CdiDevice, CdiSpec, ContainerEdits, DeviceNode};

    fn make_amd_spec(device_names: &[&str]) -> CdiSpec {
        CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: HashMap::new(),
            devices: device_names
                .iter()
                .map(|name| CdiDevice {
                    name: name.to_string(),
                    annotations: HashMap::new(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![DeviceNode {
                            path: format!(
                                "/dev/dri/renderD{}",
                                128 + name.parse::<u32>().unwrap_or(0)
                            ),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        }],
                        ..Default::default()
                    }),
                })
                .collect(),
            container_edits: None,
        }
    }

    #[test]
    fn test_add_in_memory_specs() {
        let mut cache = CdiCache::new();
        cache.add_specs(&[make_amd_spec(&["0", "1", "2"])]);

        assert_eq!(cache.len(), 3);
        assert!(cache.get_device("amd.com/gpu=0").is_some());
        assert!(cache.get_device("amd.com/gpu=1").is_some());
        assert!(cache.get_device("amd.com/gpu=2").is_some());
        assert!(cache.get_device("amd.com/gpu=99").is_none());
    }

    #[test]
    fn test_list_devices_sorted() {
        let mut cache = CdiCache::new();
        cache.add_specs(&[make_amd_spec(&["2", "0", "1"])]);

        let names = cache.list_devices();
        assert_eq!(
            names,
            vec!["amd.com/gpu=0", "amd.com/gpu=1", "amd.com/gpu=2"]
        );
    }

    #[test]
    fn test_list_vendors() {
        let mut cache = CdiCache::new();
        cache.add_specs(&[make_amd_spec(&["0"])]);

        let nvidia_spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "nvidia.com/gpu".into(),
            annotations: HashMap::new(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: HashMap::new(),
                container_edits: None,
            }],
            container_edits: None,
        };
        cache.add_specs(&[nvidia_spec]);

        let vendors = cache.list_vendors();
        assert_eq!(vendors, vec!["amd.com", "nvidia.com"]);
    }

    #[test]
    fn test_list_kinds() {
        let mut cache = CdiCache::new();
        cache.add_specs(&[make_amd_spec(&["0"])]);

        let kinds = cache.list_kinds();
        assert_eq!(kinds, vec!["amd.com/gpu"]);
    }

    #[test]
    fn test_load_from_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = CdiCache::new();
        cache.load_from_dirs(&[dir.path().to_path_buf()]);
        assert!(cache.is_empty());
        assert!(cache.get_errors().is_empty());
    }

    #[test]
    fn test_load_from_nonexistent_dir() {
        let mut cache = CdiCache::new();
        cache.load_from_dirs(&[PathBuf::from("/nonexistent/cdi/dir")]);
        assert!(cache.is_empty());
        assert!(cache.get_errors().is_empty());
    }

    #[test]
    fn test_load_json_spec_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let spec = make_amd_spec(&["0", "1"]);
        spec.write_json(&dir.path().join("amd.json")).unwrap();

        let mut cache = CdiCache::new();
        cache.load_from_dirs(&[dir.path().to_path_buf()]);

        assert_eq!(cache.len(), 2);
        assert!(cache.get_device("amd.com/gpu=0").is_some());
        assert!(cache.get_device("amd.com/gpu=1").is_some());
    }

    #[test]
    fn test_load_yaml_spec_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let spec = make_amd_spec(&["0"]);
        spec.write_yaml(&dir.path().join("amd.yaml")).unwrap();

        let mut cache = CdiCache::new();
        cache.load_from_dirs(&[dir.path().to_path_buf()]);

        assert_eq!(cache.len(), 1);
        assert!(cache.get_device("amd.com/gpu=0").is_some());
    }

    #[test]
    fn test_non_spec_files_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.txt"), "not a spec").unwrap();
        std::fs::write(dir.path().join("config.toml"), "[section]").unwrap();

        let mut cache = CdiCache::new();
        cache.load_from_dirs(&[dir.path().to_path_buf()]);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_invalid_spec_logged_but_continues() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.json"), "{ not valid json }").unwrap();

        let good = make_amd_spec(&["0"]);
        good.write_json(&dir.path().join("good.json")).unwrap();

        let mut cache = CdiCache::new();
        cache.load_from_dirs(&[dir.path().to_path_buf()]);

        assert_eq!(cache.len(), 1);
        assert!(!cache.get_errors().is_empty());
    }

    #[test]
    fn test_priority_later_dir_wins() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        let spec1 = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: HashMap::new(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: [("marker".into(), "dir1".into())].into(),
                container_edits: None,
            }],
            container_edits: None,
        };
        spec1.write_json(&dir1.path().join("amd.json")).unwrap();

        let spec2 = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: HashMap::new(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: [("marker".into(), "dir2".into())].into(),
                container_edits: None,
            }],
            container_edits: None,
        };
        spec2.write_json(&dir2.path().join("amd.json")).unwrap();

        let mut cache = CdiCache::new();
        cache.load_from_dirs(&[dir1.path().to_path_buf(), dir2.path().to_path_buf()]);

        let dev = cache.get_device("amd.com/gpu=0").unwrap();
        assert_eq!(dev.device.annotations.get("marker").unwrap(), "dir2");
    }

    #[test]
    fn test_load_uses_ondisk_when_present() {
        let dir = tempfile::tempdir().unwrap();
        make_amd_spec(&["0"])
            .write_json(&dir.path().join("amd.json"))
            .unwrap();

        let cache = CdiCache::load(&[dir.path().to_string_lossy().into_owned()], true);
        assert_eq!(cache.len(), 1);
        assert!(cache.get_device("amd.com/gpu=0").is_some());
    }

    #[test]
    fn test_load_empty_without_auto_detect() {
        let cache = CdiCache::load(&[], false);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_shared_edits_propagated() {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: HashMap::new(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: HashMap::new(),
                container_edits: None,
            }],
            container_edits: Some(ContainerEdits {
                device_nodes: vec![DeviceNode {
                    path: "/dev/kfd".into(),
                    host_path: None,
                    r#type: None,
                    major: None,
                    minor: None,
                    file_mode: None,
                    permissions: None,
                    uid: None,
                    gid: None,
                }],
                ..Default::default()
            }),
        };

        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);

        let dev = cache.get_device("amd.com/gpu=0").unwrap();
        assert!(dev.shared_edits.is_some());
        assert_eq!(dev.shared_edits.as_ref().unwrap().device_nodes.len(), 1);
        assert_eq!(
            dev.shared_edits.as_ref().unwrap().device_nodes[0].path,
            "/dev/kfd"
        );
    }
}
