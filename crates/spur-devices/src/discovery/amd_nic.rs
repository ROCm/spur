// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! AMD Pensando Pollara AI NICs: ionic PCI functions with an RDMA device.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use tracing::{debug, warn};

use crate::cdi::annotations;
use crate::cdi::spec::{CdiDevice, CdiSpec, ContainerEdits, GENERATED_CDI_VERSION};
use crate::pci::stable_id_from_bdf;
use crate::sysfs::{char_device_node, read_numa_node, read_sysfs_string, under_root};

const PENSANDO_VENDOR: &str = "0x1dd8";
const IONIC_DRIVER: &str = "ionic";
const POLLARA_TYPE: &str = "pollara400";
const RDMA_CM: &str = "/dev/infiniband/rdma_cm";

/// PCI device id to CDI kind, matching the AMD network device plugin.
fn kind_of(device: &str) -> Option<&'static str> {
    match device {
        "0x1002" => Some("amd.com/nic"),
        "0x1003" => Some("amd.com/vnic"),
        _ => None,
    }
}

struct DiscoveredNic {
    bdf: String,
    stable_id: u64,
    rdma_device: String,
    uverbs: String,
    netdev: Option<String>,
    numa_node: Option<u32>,
}

/// One spec per NIC kind present, devices in BDF order.
pub(super) fn discover_nic_cdi_from_root(root: &Path) -> Vec<CdiSpec> {
    let sysfs = root.join("sys");
    let rdma = rdma_devices_by_function(&sysfs);
    let uverbs = uverbs_by_rdma_device(&sysfs);
    let mut by_kind: BTreeMap<&str, Vec<DiscoveredNic>> = BTreeMap::new();
    let Ok(functions) = std::fs::read_dir(sysfs.join("bus/pci/devices")) else {
        return Vec::new();
    };
    for function in functions.flatten() {
        let path = function.path();
        let Some(kind) = pollara_kind(&path) else {
            continue;
        };
        if let Some(nic) = discover_nic(&path, &rdma, &uverbs) {
            by_kind.entry(kind).or_default().push(nic);
        }
    }

    let shared = shared_edits(root);
    by_kind
        .into_iter()
        .map(|(kind, mut nics)| {
            nics.sort_by(|a, b| a.bdf.cmp(&b.bdf));
            to_spec(kind, &nics, root, &shared)
        })
        .collect()
}

fn pollara_kind(function: &Path) -> Option<&'static str> {
    if read_sysfs_string(&function.join("vendor"))? != PENSANDO_VENDOR {
        return None;
    }
    let kind = kind_of(&read_sysfs_string(&function.join("device"))?)?;
    let driver = std::fs::read_link(function.join("driver")).ok()?;
    (driver.file_name()? == IONIC_DRIVER).then_some(kind)
}

/// Without an RDMA device and its uverbs node a NIC is no use to a job.
fn discover_nic(
    function: &Path,
    rdma: &HashMap<String, String>,
    uverbs: &HashMap<String, String>,
) -> Option<DiscoveredNic> {
    let bdf = function.file_name()?.to_str()?.to_owned();
    let Some(rdma_device) = rdma.get(&bdf).cloned() else {
        debug!(%bdf, "ionic function has no RDMA device; not an AI NIC");
        return None;
    };
    let Some(uverbs) = uverbs.get(&rdma_device).cloned() else {
        warn!(%bdf, %rdma_device, "AI NIC has no uverbs node (is ib_uverbs loaded?); skipping it");
        return None;
    };
    Some(DiscoveredNic {
        stable_id: stable_id_from_bdf(&bdf)?,
        uverbs,
        netdev: first_entry(&function.join("net")),
        numa_node: read_numa_node(function),
        rdma_device,
        bdf,
    })
}

/// The RDMA device of each PCI function. `ionic_rdma` may bind an auxiliary
/// device under the function, so the function is the nearest ancestor of the
/// RDMA device's parent that is named like a PCI address.
fn rdma_devices_by_function(sysfs: &Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(sysfs.join("class/infiniband")) else {
        return HashMap::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    let mut by_function = HashMap::new();
    for name in names {
        let Ok(parent) = sysfs
            .join("class/infiniband")
            .join(&name)
            .join("device")
            .canonicalize()
        else {
            continue;
        };
        let function = parent
            .ancestors()
            .filter_map(|p| p.file_name()?.to_str())
            .find(|component| stable_id_from_bdf(component).is_some());
        if let Some(function) = function {
            by_function.entry(function.to_owned()).or_insert(name);
        }
    }
    by_function
}

/// `uverbsN` numbering is independent of the RDMA device's own, so map by `ibdev`.
fn uverbs_by_rdma_device(sysfs: &Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(sysfs.join("class/infiniband_verbs")) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let ibdev = read_sysfs_string(&entry.path().join("ibdev"))?;
            Some((ibdev, entry.file_name().to_str()?.to_owned()))
        })
        .collect()
}

fn first_entry(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    names.into_iter().next()
}

fn shared_edits(root: &Path) -> Option<ContainerEdits> {
    under_root(root, RDMA_CM).exists().then(|| ContainerEdits {
        device_nodes: vec![char_device_node(root, RDMA_CM.into())],
        ..Default::default()
    })
}

fn to_spec(
    kind: &str,
    nics: &[DiscoveredNic],
    root: &Path,
    shared: &Option<ContainerEdits>,
) -> CdiSpec {
    let devices = nics
        .iter()
        .enumerate()
        .map(|(index, nic)| CdiDevice {
            name: index.to_string(),
            annotations: nic_annotations(nic),
            container_edits: Some(ContainerEdits {
                device_nodes: vec![char_device_node(
                    root,
                    format!("/dev/infiniband/{}", nic.uverbs),
                )],
                ..Default::default()
            }),
        })
        .collect();
    CdiSpec {
        cdi_version: GENERATED_CDI_VERSION.into(),
        kind: kind.into(),
        annotations: HashMap::from([(annotations::AUTO_DETECTED.into(), "true".into())]),
        devices,
        container_edits: shared.clone(),
    }
}

fn nic_annotations(nic: &DiscoveredNic) -> HashMap<String, String> {
    let mut ann = HashMap::from([
        (annotations::AUTO_DETECTED.into(), "true".into()),
        (annotations::DEVICE_TYPE.into(), POLLARA_TYPE.into()),
        (annotations::RDMA_DEVICE.into(), nic.rdma_device.clone()),
        (annotations::PCI_BDF.into(), nic.bdf.clone()),
        (annotations::STABLE_ID.into(), nic.stable_id.to_string()),
    ]);
    if let Some(netdev) = &nic.netdev {
        ann.insert(annotations::NETDEV.into(), netdev.clone());
    }
    if let Some(numa) = nic.numa_node {
        ann.insert(annotations::NUMA_NODE.into(), numa.to_string());
    }
    ann
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdi::annotations;
    use crate::cdi::cache::CdiCache;
    use crate::gres::GresCache;
    use crate::pci::stable_id_from_bdf;
    use crate::registry::{DeviceClass, DeviceRegistry};
    use std::fs;
    use std::os::unix::fs::symlink;

    struct PciFunction<'a> {
        bdf: &'a str,
        vendor: &'a str,
        device: &'a str,
        driver: Option<&'a str>,
        rdma: Option<&'a str>,
        /// Parent the RDMA device under an auxiliary device, as `ionic_rdma` binds.
        auxiliary: bool,
        netdev: Option<&'a str>,
        numa_node: &'a str,
    }

    impl<'a> PciFunction<'a> {
        fn pollara(bdf: &'a str, rdma: &'a str, netdev: &'a str) -> Self {
            Self {
                bdf,
                vendor: "0x1dd8",
                device: "0x1002",
                driver: Some("ionic"),
                rdma: Some(rdma),
                auxiliary: false,
                netdev: Some(netdev),
                numa_node: "0",
            }
        }

        fn write(&self, root: &Path) {
            let sys = root.join("sys");
            let real = sys.join("devices/pci0000:00").join(self.bdf);
            fs::create_dir_all(&real).unwrap();
            fs::write(real.join("vendor"), format!("{}\n", self.vendor)).unwrap();
            fs::write(real.join("device"), format!("{}\n", self.device)).unwrap();
            fs::write(real.join("numa_node"), format!("{}\n", self.numa_node)).unwrap();
            if let Some(driver) = self.driver {
                let dir = sys.join("bus/pci/drivers").join(driver);
                fs::create_dir_all(&dir).unwrap();
                symlink(&dir, real.join("driver")).unwrap();
            }
            if let Some(rdma) = self.rdma {
                let parent = if self.auxiliary {
                    real.join("ionic.rdma.0")
                } else {
                    real.clone()
                };
                fs::create_dir_all(parent.join("infiniband").join(rdma)).unwrap();
                let class = sys.join("class/infiniband").join(rdma);
                fs::create_dir_all(&class).unwrap();
                symlink(&parent, class.join("device")).unwrap();
            }
            if let Some(netdev) = self.netdev {
                fs::create_dir_all(real.join("net").join(netdev)).unwrap();
            }
            let bus = sys.join("bus/pci/devices");
            fs::create_dir_all(&bus).unwrap();
            symlink(&real, bus.join(self.bdf)).unwrap();
        }
    }

    fn uverbs(root: &Path, uverbs: &str, ibdev: &str) {
        let class = root.join("sys/class/infiniband_verbs").join(uverbs);
        fs::create_dir_all(&class).unwrap();
        fs::write(class.join("ibdev"), format!("{ibdev}\n")).unwrap();
        let dev = root.join("dev/infiniband");
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join(uverbs), "").unwrap();
    }

    /// Two PFs and a VF, with uverbs numbers that differ from the ionic ones.
    fn pollara_host() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        PciFunction::pollara("0000:85:00.0", "ionic_1", "enp133s0").write(root);
        PciFunction::pollara("0000:05:00.0", "ionic_0", "enp5s0").write(root);
        PciFunction {
            device: "0x1003",
            numa_node: "1",
            ..PciFunction::pollara("0000:05:00.1", "ionic_2", "enp5s0v0")
        }
        .write(root);
        uverbs(root, "uverbs0", "ionic_1");
        uverbs(root, "uverbs1", "ionic_0");
        uverbs(root, "uverbs2", "ionic_2");
        fs::write(root.join("dev/infiniband/rdma_cm"), "").unwrap();
        dir
    }

    fn device_nodes(spec: &CdiSpec) -> Vec<Vec<String>> {
        spec.devices
            .iter()
            .map(|d| {
                d.container_edits
                    .iter()
                    .flat_map(|e| &e.device_nodes)
                    .map(|n| n.path.clone())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn discovers_pfs_and_vfs_as_separate_kinds() {
        let host = pollara_host();
        let specs = discover_nic_cdi_from_root(host.path());
        let kinds: Vec<(&str, usize)> = specs
            .iter()
            .map(|s| (s.kind.as_str(), s.devices.len()))
            .collect();
        assert_eq!(kinds, vec![("amd.com/nic", 2), ("amd.com/vnic", 1)]);
    }

    #[test]
    fn uverbs_is_mapped_through_ibdev_not_index() {
        let host = pollara_host();
        let specs = discover_nic_cdi_from_root(host.path());
        assert_eq!(
            device_nodes(&specs[0]),
            vec![
                vec!["/dev/infiniband/uverbs1".to_string()],
                vec!["/dev/infiniband/uverbs0".to_string()],
            ],
            "PFs in BDF order, each with the uverbs node of its own RDMA device"
        );
        assert_eq!(
            device_nodes(&specs[1]),
            vec![vec!["/dev/infiniband/uverbs2".to_string()]]
        );
    }

    #[test]
    fn each_nic_carries_its_identity_in_annotations() {
        let host = pollara_host();
        let specs = discover_nic_cdi_from_root(host.path());
        let vf = &specs[1].devices[0].annotations;
        let get = |key: &str| vf.get(key).map(String::as_str);
        assert_eq!(get(annotations::DEVICE_TYPE), Some("pollara400"));
        assert_eq!(get(annotations::RDMA_DEVICE), Some("ionic_2"));
        assert_eq!(get(annotations::NETDEV), Some("enp5s0v0"));
        assert_eq!(get(annotations::PCI_BDF), Some("0000:05:00.1"));
        assert_eq!(get(annotations::NUMA_NODE), Some("1"));
        let stable_id = stable_id_from_bdf("0000:05:00.1").unwrap().to_string();
        assert_eq!(get(annotations::STABLE_ID), Some(stable_id.as_str()));
        assert_eq!(get(annotations::AUTO_DETECTED), Some("true"));
    }

    #[test]
    fn every_kind_shares_the_rdma_connection_manager() {
        let host = pollara_host();
        let specs = discover_nic_cdi_from_root(host.path());
        assert_eq!(specs.len(), 2);
        for spec in specs {
            let shared: Vec<&str> = spec
                .container_edits
                .iter()
                .flat_map(|e| &e.device_nodes)
                .map(|n| n.path.as_str())
                .collect();
            assert_eq!(shared, vec!["/dev/infiniband/rdma_cm"], "{}", spec.kind);
        }
    }

    #[test]
    fn an_rdma_device_under_an_auxiliary_device_belongs_to_its_function() {
        let dir = tempfile::tempdir().unwrap();
        PciFunction {
            auxiliary: true,
            ..PciFunction::pollara("0000:05:00.0", "ionic_0", "enp5s0")
        }
        .write(dir.path());
        uverbs(dir.path(), "uverbs3", "ionic_0");
        let specs = discover_nic_cdi_from_root(dir.path());
        assert_eq!(
            device_nodes(&specs[0]),
            vec![vec!["/dev/infiniband/uverbs3".to_string()]]
        );
        assert_eq!(
            specs[0].devices[0]
                .annotations
                .get(annotations::PCI_BDF)
                .map(String::as_str),
            Some("0000:05:00.0")
        );
    }

    #[test]
    fn ionic_without_rdma_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        PciFunction {
            rdma: None,
            ..PciFunction::pollara("0000:05:00.0", "ionic_0", "enp5s0")
        }
        .write(dir.path());
        uverbs(dir.path(), "uverbs0", "ionic_0");
        assert!(discover_nic_cdi_from_root(dir.path()).is_empty());
    }

    #[test]
    fn only_ionic_bound_pensando_functions_are_nics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        PciFunction {
            driver: Some("vfio-pci"),
            ..PciFunction::pollara("0000:05:00.0", "ionic_0", "enp5s0")
        }
        .write(root);
        PciFunction {
            driver: None,
            ..PciFunction::pollara("0000:06:00.0", "ionic_1", "enp6s0")
        }
        .write(root);
        PciFunction {
            vendor: "0x15b3",
            ..PciFunction::pollara("0000:07:00.0", "mlx5_0", "enp7s0")
        }
        .write(root);
        PciFunction {
            device: "0x1000",
            ..PciFunction::pollara("0000:08:00.0", "ionic_3", "enp8s0")
        }
        .write(root);
        uverbs(root, "uverbs0", "ionic_0");
        uverbs(root, "uverbs1", "ionic_1");
        uverbs(root, "uverbs2", "mlx5_0");
        uverbs(root, "uverbs3", "ionic_3");
        assert!(discover_nic_cdi_from_root(root).is_empty());
    }

    #[test]
    fn a_nic_without_a_uverbs_node_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        PciFunction::pollara("0000:05:00.0", "ionic_0", "enp5s0").write(dir.path());
        assert!(discover_nic_cdi_from_root(dir.path()).is_empty());
    }

    #[test]
    fn host_discovery_includes_nics() {
        let host = pollara_host();
        let kinds: Vec<String> = crate::discovery::discover_all_cdi_from_root(host.path())
            .into_iter()
            .map(|s| s.kind)
            .collect();
        assert_eq!(kinds, vec!["amd.com/nic", "amd.com/vnic"]);
    }

    #[test]
    fn discovered_nics_register_as_injectable_nic_gres() {
        let host = pollara_host();
        let specs = discover_nic_cdi_from_root(host.path());
        let mut registry = DeviceRegistry::new();
        registry.populate(
            &CdiCache::load_with(&[], Some(&specs)),
            &GresCache::default(),
        );

        let mut nics: Vec<_> = registry
            .list()
            .iter()
            .filter(|e| e.gres_name == "nic")
            .collect();
        nics.sort_by_key(|e| e.device_id);
        assert_eq!(nics.len(), 2);
        assert!(nics.iter().all(|e| e.is_injectable()
            && e.class() == DeviceClass::Nic
            && e.resource_type.as_deref() == Some("pollara400")
            && e.inject_actions.gpu_visibility_vars.is_empty()));
        assert_eq!(nics[0].rdma_device.as_deref(), Some("ionic_0"));
        assert_eq!(
            nics[0].stable_id,
            stable_id_from_bdf("0000:05:00.0").unwrap()
        );
        assert!(registry.has_injectable("vnic"));
    }
}
