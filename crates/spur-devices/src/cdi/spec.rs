// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const GENERATED_CDI_VERSION: &str = "0.6.0";

const MIN_CDI_VERSION: &str = "0.3.0";

const MAX_CDI_VERSION: &str = "1.1.0";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CdiSpec {
    #[serde(rename = "cdiVersion")]
    pub cdi_version: String,

    pub kind: String,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,

    pub devices: Vec<CdiDevice>,

    #[serde(
        rename = "containerEdits",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub container_edits: Option<ContainerEdits>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CdiDevice {
    pub name: String,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,

    #[serde(
        rename = "containerEdits",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub container_edits: Option<ContainerEdits>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ContainerEdits {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,

    #[serde(rename = "deviceNodes", default, skip_serializing_if = "Vec::is_empty")]
    pub device_nodes: Vec<DeviceNode>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<Mount>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<Hook>,

    #[serde(
        rename = "additionalGids",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub additional_gids: Vec<u32>,

    // v0.7.0+
    #[serde(rename = "intelRdt", default, skip_serializing_if = "Option::is_none")]
    pub intel_rdt: Option<IntelRdt>,

    // v1.1.0+
    #[serde(rename = "netDevices", default, skip_serializing_if = "Vec::is_empty")]
    pub net_devices: Vec<NetDevice>,
}

impl ContainerEdits {
    pub fn is_empty(&self) -> bool {
        self.env.is_empty()
            && self.device_nodes.is_empty()
            && self.mounts.is_empty()
            && self.hooks.is_empty()
            && self.additional_gids.is_empty()
            && self.intel_rdt.is_none()
            && self.net_devices.is_empty()
    }

    pub fn append(&mut self, other: &ContainerEdits) {
        self.env.extend_from_slice(&other.env);
        self.device_nodes.extend_from_slice(&other.device_nodes);
        self.mounts.extend_from_slice(&other.mounts);
        self.hooks.extend_from_slice(&other.hooks);
        self.additional_gids
            .extend_from_slice(&other.additional_gids);
        if other.intel_rdt.is_some() {
            self.intel_rdt.clone_from(&other.intel_rdt);
        }
        self.net_devices.extend_from_slice(&other.net_devices);
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DeviceNode {
    pub path: String,

    #[serde(rename = "hostPath", default, skip_serializing_if = "Option::is_none")]
    pub host_path: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub major: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minor: Option<i64>,

    #[serde(rename = "fileMode", default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
}

impl DeviceNode {
    pub fn effective_host_path(&self) -> &str {
        self.host_path.as_deref().unwrap_or(&self.path)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Mount {
    #[serde(rename = "hostPath")]
    pub host_path: String,

    #[serde(rename = "containerPath")]
    pub container_path: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Hook {
    #[serde(rename = "hookName")]
    pub hook_name: String,

    pub path: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct IntelRdt {
    #[serde(rename = "closID", default, skip_serializing_if = "Option::is_none")]
    pub clos_id: Option<String>,

    #[serde(
        rename = "l3CacheSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub l3_cache_schema: Option<String>,

    #[serde(
        rename = "memBwSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub mem_bw_schema: Option<String>,

    // v1.1.0+
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schemata: Vec<String>,

    // v1.1.0+
    #[serde(
        rename = "enableMonitoring",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub enable_monitoring: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NetDevice {
    #[serde(rename = "hostInterfaceName")]
    pub host_interface_name: String,

    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QualifiedName {
    pub vendor: String,
    pub class: String,
    pub device: String,
}

impl QualifiedName {
    pub fn kind(&self) -> String {
        format!("{}/{}", self.vendor, self.class)
    }
}

impl std::fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}={}", self.vendor, self.class, self.device)
    }
}

pub fn parse_qualified_name(name: &str) -> Result<QualifiedName> {
    let (kind, device) = name
        .split_once('=')
        .with_context(|| format!("invalid CDI device name '{}': missing '='", name))?;

    let (vendor, class) = kind
        .split_once('/')
        .with_context(|| format!("invalid CDI device name '{}': missing '/' in kind", name))?;

    if vendor.is_empty() || class.is_empty() || device.is_empty() {
        bail!(
            "invalid CDI device name '{}': empty vendor, class, or device",
            name
        );
    }

    Ok(QualifiedName {
        vendor: vendor.to_owned(),
        class: class.to_owned(),
        device: device.to_owned(),
    })
}

pub fn is_qualified_name(name: &str) -> bool {
    parse_qualified_name(name).is_ok()
}

impl CdiSpec {
    pub fn read_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading CDI spec from {}", path.display()))?;

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let spec: CdiSpec = match ext {
            "json" => serde_json::from_str(&content)
                .with_context(|| format!("parsing JSON CDI spec {}", path.display()))?,
            "yaml" | "yml" => serde_yaml::from_str(&content)
                .with_context(|| format!("parsing YAML CDI spec {}", path.display()))?,
            _ => serde_yaml::from_str(&content)
                .or_else(|_| serde_json::from_str(&content))
                .with_context(|| {
                    format!("parsing CDI spec {} (unknown extension)", path.display())
                })?,
        };

        Ok(spec)
    }

    pub fn write_json(&self, path: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self).context("serializing CDI spec to JSON")?;
        std::fs::write(path, content)
            .with_context(|| format!("writing CDI spec to {}", path.display()))?;
        Ok(())
    }

    pub fn write_yaml(&self, path: &Path) -> Result<()> {
        let content = serde_yaml::to_string(self).context("serializing CDI spec to YAML")?;
        std::fs::write(path, content)
            .with_context(|| format!("writing CDI spec to {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_version()?;
        self.validate_kind()?;

        if self.devices.is_empty() {
            bail!("CDI spec must contain at least one device");
        }

        for dev in &self.devices {
            if dev.name.is_empty() {
                bail!("CDI device name must not be empty");
            }
        }

        Ok(())
    }

    fn validate_version(&self) -> Result<()> {
        let version = semver::Version::parse(&self.cdi_version)
            .with_context(|| format!("invalid cdiVersion '{}'", self.cdi_version))?;
        let min = semver::Version::parse(MIN_CDI_VERSION)
            .context("internal: MIN_CDI_VERSION is not valid semver")?;
        let max = semver::Version::parse(MAX_CDI_VERSION)
            .context("internal: MAX_CDI_VERSION is not valid semver")?;

        if version < min || version > max {
            bail!(
                "cdiVersion '{}' out of supported range ({} - {})",
                self.cdi_version,
                MIN_CDI_VERSION,
                MAX_CDI_VERSION
            );
        }

        Ok(())
    }

    fn validate_kind(&self) -> Result<()> {
        if !self.kind.contains('/') {
            bail!(
                "invalid CDI kind '{}': must be in vendor/class format",
                self.kind
            );
        }
        Ok(())
    }

    pub fn parse_kind(&self) -> Option<(&str, &str)> {
        self.kind.split_once('/')
    }

    pub fn qualified_name(&self, device_name: &str) -> String {
        format!("{}={}", self.kind, device_name)
    }

    pub fn device_node_paths(&self, device_name: &str) -> Vec<String> {
        let mut paths = Vec::new();

        if let Some(dev) = self.devices.iter().find(|d| d.name == device_name) {
            if let Some(ref edits) = dev.container_edits {
                for dn in &edits.device_nodes {
                    paths.push(dn.effective_host_path().to_owned());
                }
            }
        }

        paths
    }
}

pub fn vendor_from_kind(kind: &str) -> Option<String> {
    let (prefix, class) = kind.split_once('/')?;
    if prefix.is_empty() || class.is_empty() {
        return None;
    }
    let label = prefix.split('.').next()?;
    if label.is_empty() {
        return None;
    }
    Some(label.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vendor_from_kind() {
        assert_eq!(vendor_from_kind("amd.com/gpu").as_deref(), Some("amd"));
        assert_eq!(
            vendor_from_kind("nvidia.com/gpu").as_deref(),
            Some("nvidia")
        );
        assert_eq!(vendor_from_kind("intel.com/gpu").as_deref(), Some("intel"));
        assert_eq!(vendor_from_kind("invalid").as_deref(), None);
        assert_eq!(vendor_from_kind("/gpu").as_deref(), None);
    }

    #[test]
    fn test_parse_qualified_name() {
        let qn = parse_qualified_name("amd.com/gpu=0").unwrap();
        assert_eq!(qn.vendor, "amd.com");
        assert_eq!(qn.class, "gpu");
        assert_eq!(qn.device, "0");
        assert_eq!(qn.kind(), "amd.com/gpu");
    }

    #[test]
    fn test_parse_qualified_name_nvidia() {
        let qn = parse_qualified_name("nvidia.com/gpu=all").unwrap();
        assert_eq!(qn.vendor, "nvidia.com");
        assert_eq!(qn.class, "gpu");
        assert_eq!(qn.device, "all");
    }

    #[test]
    fn test_parse_qualified_name_invalid_no_equals() {
        assert!(parse_qualified_name("amd.com/gpu").is_err());
    }

    #[test]
    fn test_parse_qualified_name_invalid_no_slash() {
        assert!(parse_qualified_name("amd.com=0").is_err());
    }

    #[test]
    fn test_parse_qualified_name_invalid_empty_parts() {
        assert!(parse_qualified_name("/gpu=0").is_err());
        assert!(parse_qualified_name("amd.com/=0").is_err());
        assert!(parse_qualified_name("amd.com/gpu=").is_err());
    }

    #[test]
    fn test_is_qualified_name() {
        assert!(is_qualified_name("amd.com/gpu=0"));
        assert!(is_qualified_name("nvidia.com/gpu=all"));
        assert!(!is_qualified_name("gpu:mi300x:4"));
        assert!(!is_qualified_name("/dev/nvidia0"));
    }

    #[test]
    fn test_spec_roundtrip_json() {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: HashMap::new(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: HashMap::new(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![DeviceNode {
                        path: "/dev/dri/renderD128".into(),
                        host_path: None,
                        r#type: Some("c".into()),
                        major: Some(226),
                        minor: Some(128),
                        file_mode: None,
                        permissions: Some("rw".into()),
                        uid: None,
                        gid: None,
                    }],
                    ..Default::default()
                }),
            }],
            container_edits: Some(ContainerEdits {
                device_nodes: vec![DeviceNode {
                    path: "/dev/kfd".into(),
                    host_path: None,
                    r#type: Some("c".into()),
                    major: None,
                    minor: None,
                    file_mode: None,
                    permissions: Some("rw".into()),
                    uid: None,
                    gid: None,
                }],
                ..Default::default()
            }),
        };

        let json = serde_json::to_string_pretty(&spec).unwrap();
        let parsed: CdiSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, parsed);
    }

    #[test]
    fn test_spec_roundtrip_yaml() {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "nvidia.com/gpu".into(),
            annotations: HashMap::new(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: HashMap::new(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![DeviceNode {
                        path: "/dev/nvidia0".into(),
                        host_path: None,
                        r#type: Some("c".into()),
                        major: Some(195),
                        minor: Some(0),
                        file_mode: None,
                        permissions: None,
                        uid: None,
                        gid: None,
                    }],
                    env: vec!["NVIDIA_VISIBLE_DEVICES=void".into()],
                    ..Default::default()
                }),
            }],
            container_edits: None,
        };

        let yaml = serde_yaml::to_string(&spec).unwrap();
        let parsed: CdiSpec = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(spec, parsed);
    }

    #[test]
    fn test_validate_version_range() {
        let mut spec = make_minimal_spec();

        spec.cdi_version = "0.6.0".into();
        assert!(spec.validate().is_ok());

        spec.cdi_version = "1.1.0".into();
        assert!(spec.validate().is_ok());

        spec.cdi_version = "0.3.0".into();
        assert!(spec.validate().is_ok());

        spec.cdi_version = "0.2.0".into();
        assert!(spec.validate().is_err());

        spec.cdi_version = "2.0.0".into();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn test_validate_kind_format() {
        let mut spec = make_minimal_spec();
        spec.kind = "no-slash".into();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn test_validate_empty_devices() {
        let mut spec = make_minimal_spec();
        spec.devices.clear();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn test_container_edits_append() {
        let mut a = ContainerEdits {
            env: vec!["A=1".into()],
            device_nodes: vec![DeviceNode {
                path: "/dev/a".into(),
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
        };
        let b = ContainerEdits {
            env: vec!["B=2".into()],
            device_nodes: vec![DeviceNode {
                path: "/dev/b".into(),
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
        };
        a.append(&b);
        assert_eq!(a.env, vec!["A=1", "B=2"]);
        assert_eq!(a.device_nodes.len(), 2);
    }

    #[test]
    fn test_container_edits_is_empty() {
        assert!(ContainerEdits::default().is_empty());
        assert!(!ContainerEdits {
            env: vec!["X=1".into()],
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn test_device_node_effective_host_path() {
        let with_host = DeviceNode {
            path: "/dev/dri/renderD128".into(),
            host_path: Some("/host/dev/dri/renderD128".into()),
            r#type: None,
            major: None,
            minor: None,
            file_mode: None,
            permissions: None,
            uid: None,
            gid: None,
        };
        assert_eq!(with_host.effective_host_path(), "/host/dev/dri/renderD128");

        let without_host = DeviceNode {
            path: "/dev/dri/renderD128".into(),
            host_path: None,
            r#type: None,
            major: None,
            minor: None,
            file_mode: None,
            permissions: None,
            uid: None,
            gid: None,
        };
        assert_eq!(without_host.effective_host_path(), "/dev/dri/renderD128");
    }

    #[test]
    fn test_spec_qualified_name() {
        let spec = make_minimal_spec();
        assert_eq!(spec.qualified_name("0"), "amd.com/gpu=0");
    }

    #[test]
    fn test_read_write_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");

        let spec = make_minimal_spec();
        spec.write_json(&path).unwrap();

        let loaded = CdiSpec::read_file(&path).unwrap();
        assert_eq!(spec, loaded);
    }

    #[test]
    fn test_read_write_yaml_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");

        let spec = make_minimal_spec();
        spec.write_yaml(&path).unwrap();

        let loaded = CdiSpec::read_file(&path).unwrap();
        assert_eq!(spec, loaded);
    }

    fn make_minimal_spec() -> CdiSpec {
        CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: HashMap::new(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: HashMap::new(),
                container_edits: None,
            }],
            container_edits: None,
        }
    }
}
