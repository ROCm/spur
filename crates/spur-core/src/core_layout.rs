// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::config::NodeConfig;
use crate::cpu_topology::{parse_cpu_list, CpuListError, CpuTopology};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuReservedCpus {
    pub stable_id: u64,
    pub cpus: Vec<u32>,
}

/// Controller-computed split of a node's CPUs. All CPU lists are sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoreLayout {
    pub spec_cpus: Vec<u32>,
    pub mem_spec_mb: u64,
    pub gpu_reserved: Vec<GpuReservedCpus>,
    pub general_cpus: Vec<u32>,
    pub daemons_off_spec: bool,
}

/// The `[[nodes]]` settings that shape a node's core layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeCoreSettings {
    pub core_spec_count: Option<u32>,
    pub cpu_spec_list: Option<String>,
    pub reserved_memory_mb: u64,
    pub daemons_off_spec: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LayoutError {
    #[error("no CPU topology from spurd; restart or upgrade spurd")]
    MissingTopology,
    #[error("cpu_spec_list: {0}")]
    BadSpecList(#[from] CpuListError),
    #[error("cpu_spec_list names CPU {0}, which is not online")]
    SpecCpuNotOnline(u32),
    #[error("spec cores would take all {cores} cores, leaving none for jobs")]
    SpecTakesAllCores { cores: usize },
}

enum SpecRequest<'a> {
    Count(u32),
    List(&'a str),
}

impl NodeCoreSettings {
    pub fn from_config(nc: &NodeConfig) -> Self {
        NodeCoreSettings {
            core_spec_count: nc.core_spec_count,
            cpu_spec_list: nc.cpu_spec_list.clone(),
            reserved_memory_mb: nc.reserved_memory_mb,
            daemons_off_spec: nc.daemons_off_spec,
        }
    }

    fn spec_request(&self) -> Option<SpecRequest<'_>> {
        if let Some(n) = self.core_spec_count.filter(|n| *n > 0) {
            return Some(SpecRequest::Count(n));
        }
        self.cpu_spec_list
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(SpecRequest::List)
    }
}

impl CoreLayout {
    /// `Ok(None)` when the node sets no spec cores: it behaves exactly as before.
    pub fn compute(
        topology: Option<&CpuTopology>,
        settings: &NodeCoreSettings,
    ) -> Result<Option<CoreLayout>, LayoutError> {
        let Some(spec) = settings.spec_request() else {
            return Ok(None);
        };
        let topology = topology.ok_or(LayoutError::MissingTopology)?;
        let spec_cores = match spec {
            SpecRequest::Count(n) => pick_spec_cores(topology, n),
            SpecRequest::List(list) => cores_of_cpus(topology, &parse_cpu_list(list)?)?,
        };
        if spec_cores.len() >= topology.cores.len() {
            return Err(LayoutError::SpecTakesAllCores {
                cores: topology.cores.len(),
            });
        }
        let mut spec_cpus = Vec::new();
        let mut general_cpus = Vec::new();
        for (i, core) in topology.cores.iter().enumerate() {
            let pool = if spec_cores.contains(&i) {
                &mut spec_cpus
            } else {
                &mut general_cpus
            };
            pool.extend(core.cpus.iter().copied());
        }
        spec_cpus.sort_unstable();
        general_cpus.sort_unstable();
        Ok(Some(CoreLayout {
            spec_cpus,
            mem_spec_mb: settings.reserved_memory_mb,
            gpu_reserved: Vec::new(),
            general_cpus,
            daemons_off_spec: settings.daemons_off_spec,
        }))
    }

    /// FNV-1a 64 over: spec_cpus, mem_spec_mb, gpu_reserved (sorted by
    /// stable_id; each stable_id then its cpus), general_cpus, daemons_off_spec.
    /// Each list is its length (u64) then its items (u32); integers are
    /// little-endian; the bool is one byte. Stable across builds by design.
    pub fn fingerprint(&self) -> u64 {
        let mut h = Fnv::new();
        h.list(&self.spec_cpus);
        h.u64(self.mem_spec_mb);
        let mut gpus: Vec<&GpuReservedCpus> = self.gpu_reserved.iter().collect();
        gpus.sort_by_key(|g| g.stable_id);
        h.u64(gpus.len() as u64);
        for g in gpus {
            h.u64(g.stable_id);
            h.list(&g.cpus);
        }
        h.list(&self.general_cpus);
        h.bytes(&[u8::from(self.daemons_off_spec)]);
        nonzero(h.finish())
    }
}

/// 0 on the wire means "no layout held", so a layout must never hash to it.
fn nonzero(hash: u64) -> u64 {
    if hash == 0 {
        1
    } else {
        hash
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
    fn list(&mut self, v: &[u32]) {
        self.u64(v.len() as u64);
        for x in v {
            self.bytes(&x.to_le_bytes());
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Slurm's order: the highest local core index first, sockets from highest to
/// lowest at each index (`node_conf_select_spec_cores`).
fn pick_spec_cores(topology: &CpuTopology, count: u32) -> BTreeSet<usize> {
    let mut by_socket: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, core) in topology.cores.iter().enumerate() {
        by_socket.entry(core.socket).or_default().push(i);
    }
    let widest = by_socket.values().map(Vec::len).max().unwrap_or(0);
    let mut picked = BTreeSet::new();
    for local in (0..widest).rev() {
        for cores in by_socket.values().rev() {
            if picked.len() >= count as usize {
                return picked;
            }
            if let Some(&i) = cores.get(local) {
                picked.insert(i);
            }
        }
    }
    picked
}

fn cores_of_cpus(topology: &CpuTopology, cpus: &[u32]) -> Result<BTreeSet<usize>, LayoutError> {
    cpus.iter()
        .map(|&cpu| {
            topology
                .core_of_cpu(cpu)
                .ok_or(LayoutError::SpecCpuNotOnline(cpu))
        })
        .collect()
}

/// `[[nodes]]` entries (by `names`, or the sorted selector when `names` is
/// empty) whose restart-only core settings differ between two configs.
pub fn restart_only_node_changes(old: &[NodeConfig], new: &[NodeConfig]) -> Vec<String> {
    let index = |nodes: &[NodeConfig]| -> BTreeMap<String, NodeCoreSettings> {
        nodes
            .iter()
            .map(|nc| (entry_key(nc), NodeCoreSettings::from_config(nc)))
            .collect()
    };
    let (old, new) = (index(old), index(new));
    let unset = NodeCoreSettings {
        daemons_off_spec: true,
        ..Default::default()
    };
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    keys.into_iter()
        .filter(|k| old.get(*k).unwrap_or(&unset) != new.get(*k).unwrap_or(&unset))
        .cloned()
        .collect()
}

fn entry_key(nc: &NodeConfig) -> String {
    if !nc.names.is_empty() {
        return nc.names.clone();
    }
    let selector: BTreeMap<&String, &String> = nc.selector.iter().collect();
    format!("{selector:?}")
}

impl From<&CoreLayout> for spur_proto::proto::CoreLayout {
    fn from(l: &CoreLayout) -> Self {
        spur_proto::proto::CoreLayout {
            spec_cpus: l.spec_cpus.clone(),
            mem_spec_mb: l.mem_spec_mb,
            gpu_reserved: l
                .gpu_reserved
                .iter()
                .map(|g| spur_proto::proto::GpuReservedCpus {
                    stable_id: g.stable_id,
                    cpus: g.cpus.clone(),
                })
                .collect(),
            general_cpus: l.general_cpus.clone(),
            daemons_off_spec: l.daemons_off_spec,
        }
    }
}

impl From<spur_proto::proto::CoreLayout> for CoreLayout {
    fn from(l: spur_proto::proto::CoreLayout) -> Self {
        CoreLayout {
            spec_cpus: l.spec_cpus,
            mem_spec_mb: l.mem_spec_mb,
            gpu_reserved: l
                .gpu_reserved
                .into_iter()
                .map(|g| GpuReservedCpus {
                    stable_id: g.stable_id,
                    cpus: g.cpus,
                })
                .collect(),
            general_cpus: l.general_cpus,
            daemons_off_spec: l.daemons_off_spec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_topology::{CoreInfo, CpuTopology};

    /// Golden value: FNV-1a 64 over the canonical encoding documented on
    /// `CoreLayout::fingerprint`, computed independently in Python. Pins
    /// cross-build stability.
    const GOLDEN_FINGERPRINT: u64 = 0x2f4ffdf52174e0df;

    /// 2 sockets x 64 cores x 2 threads: socket s, local core c -> CPUs s*64+c, s*64+c+128.
    fn example_node() -> CpuTopology {
        let mut cores = Vec::new();
        for s in 0..2u32 {
            for c in 0..64u32 {
                let cpu = s * 64 + c;
                cores.push(CoreInfo {
                    socket: s,
                    numa_node: Some(s),
                    cpus: vec![cpu, cpu + 128],
                });
            }
        }
        CpuTopology { cores }
    }

    fn spec_count(n: u32) -> NodeCoreSettings {
        NodeCoreSettings {
            core_spec_count: Some(n),
            reserved_memory_mb: 16384,
            daemons_off_spec: true,
            ..Default::default()
        }
    }

    fn spec_list(list: &str) -> NodeCoreSettings {
        NodeCoreSettings {
            cpu_spec_list: Some(list.into()),
            daemons_off_spec: true,
            ..Default::default()
        }
    }

    #[test]
    fn core_spec_count_one_takes_the_highest_core_on_the_highest_socket() {
        let layout = CoreLayout::compute(Some(&example_node()), &spec_count(1))
            .unwrap()
            .unwrap();
        assert_eq!(layout.spec_cpus, vec![127, 255]);
        assert_eq!(layout.general_cpus.len(), 254);
        assert!(!layout.general_cpus.contains(&127));
        assert_eq!(layout.mem_spec_mb, 16384);
        assert!(layout.daemons_off_spec);
        assert!(layout.gpu_reserved.is_empty());
    }

    #[test]
    fn core_spec_count_round_robins_down_across_sockets() {
        let layout = CoreLayout::compute(Some(&example_node()), &spec_count(3))
            .unwrap()
            .unwrap();
        // core 63 of socket 1, core 63 of socket 0, then core 62 of socket 1.
        assert_eq!(layout.spec_cpus, vec![63, 126, 127, 191, 254, 255]);
    }

    #[test]
    fn non_smt_and_single_socket_nodes_follow_the_same_order() {
        let topo = CpuTopology {
            cores: (0..4)
                .map(|c| CoreInfo {
                    socket: 0,
                    numa_node: None,
                    cpus: vec![c],
                })
                .collect(),
        };
        let layout = CoreLayout::compute(Some(&topo), &spec_count(2))
            .unwrap()
            .unwrap();
        assert_eq!(layout.spec_cpus, vec![2, 3]);
        assert_eq!(layout.general_cpus, vec![0, 1]);
    }

    #[test]
    fn cpu_spec_list_is_widened_to_whole_cores() {
        let one = CoreLayout::compute(Some(&example_node()), &spec_list("127"))
            .unwrap()
            .unwrap();
        let both = CoreLayout::compute(Some(&example_node()), &spec_list("127,255"))
            .unwrap()
            .unwrap();
        assert_eq!(one.spec_cpus, vec![127, 255]);
        assert_eq!(one, both);
    }

    #[test]
    fn unset_zero_blank_and_memory_only_settings_give_no_layout() {
        let topo = example_node();
        for settings in [
            NodeCoreSettings::default(),
            spec_count(0),
            spec_list("  "),
            NodeCoreSettings {
                reserved_memory_mb: 16384,
                ..Default::default()
            },
        ] {
            assert_eq!(
                CoreLayout::compute(Some(&topo), &settings),
                Ok(None),
                "{settings:?}"
            );
        }
        assert_eq!(
            CoreLayout::compute(None, &NodeCoreSettings::default()),
            Ok(None)
        );
    }

    #[test]
    fn layout_errors_name_the_problem() {
        let topo = example_node();
        assert_eq!(
            CoreLayout::compute(None, &spec_count(1)),
            Err(LayoutError::MissingTopology)
        );
        assert_eq!(
            CoreLayout::compute(Some(&topo), &spec_list("300")),
            Err(LayoutError::SpecCpuNotOnline(300))
        );
        assert_eq!(
            CoreLayout::compute(Some(&topo), &spec_count(128)),
            Err(LayoutError::SpecTakesAllCores { cores: 128 })
        );
        assert_eq!(
            CoreLayout::compute(Some(&topo), &spec_count(500)),
            Err(LayoutError::SpecTakesAllCores { cores: 128 })
        );
        assert!(LayoutError::MissingTopology
            .to_string()
            .contains("restart or upgrade spurd"));
    }

    #[test]
    fn fingerprint_is_deterministic_and_tracks_changes() {
        let topo = example_node();
        let a = CoreLayout::compute(Some(&topo), &spec_count(1))
            .unwrap()
            .unwrap();
        let b = CoreLayout::compute(Some(&topo), &spec_count(1))
            .unwrap()
            .unwrap();
        let c = CoreLayout::compute(Some(&topo), &spec_count(2))
            .unwrap()
            .unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_ne!(a.fingerprint(), 0);
    }

    #[test]
    fn fingerprint_matches_the_documented_encoding() {
        // Golden value: FNV-1a 64 over the canonical encoding (see
        // `CoreLayout::fingerprint`). Pins cross-build stability.
        let layout = CoreLayout {
            spec_cpus: vec![127, 255],
            mem_spec_mb: 16384,
            gpu_reserved: vec![],
            general_cpus: vec![0, 1],
            daemons_off_spec: true,
        };
        assert_eq!(layout.fingerprint(), GOLDEN_FINGERPRINT);
    }

    #[test]
    fn a_zero_hash_is_reported_as_nonzero() {
        assert_eq!(nonzero(0), 1);
        assert_eq!(nonzero(42), 42);
    }

    #[test]
    fn layout_survives_the_proto_round_trip() {
        let layout = CoreLayout {
            spec_cpus: vec![127, 255],
            mem_spec_mb: 16384,
            gpu_reserved: vec![GpuReservedCpus {
                stable_id: 7,
                cpus: vec![0, 128],
            }],
            general_cpus: vec![1, 129],
            daemons_off_spec: false,
        };
        let wire = spur_proto::proto::CoreLayout::from(&layout);
        assert_eq!(CoreLayout::from(wire), layout);
    }

    #[test]
    fn settings_come_from_the_nodes_entry() {
        let nc: crate::config::NodeConfig = toml::from_str(
            r#"
            names = "n1"
            core_spec_count = 1
            mem_spec_limit = 2048
            daemons_off_spec = false
            "#,
        )
        .unwrap();
        assert_eq!(
            NodeCoreSettings::from_config(&nc),
            NodeCoreSettings {
                core_spec_count: Some(1),
                cpu_spec_list: None,
                reserved_memory_mb: 2048,
                daemons_off_spec: false
            }
        );
    }

    #[test]
    fn restart_only_changes_are_reported_per_nodes_entry() {
        let parse = |s: &str| -> Vec<crate::config::NodeConfig> {
            #[derive(serde::Deserialize)]
            struct Nodes {
                nodes: Vec<crate::config::NodeConfig>,
            }
            toml::from_str::<Nodes>(s).unwrap().nodes
        };
        let old = parse(
            "[[nodes]]\nnames = \"a\"\ncore_spec_count = 1\n[[nodes]]\nnames = \"b\"\nweight = 2\n",
        );
        let same_but_weight = parse(
            "[[nodes]]\nnames = \"a\"\ncore_spec_count = 1\n[[nodes]]\nnames = \"b\"\nweight = 5\n",
        );
        assert!(restart_only_node_changes(&old, &same_but_weight).is_empty());
        let changed = parse("[[nodes]]\nnames = \"a\"\ncore_spec_count = 2\n[[nodes]]\nnames = \"b\"\nreserved_memory_mb = 512\n");
        assert_eq!(
            restart_only_node_changes(&old, &changed),
            vec!["a".to_string(), "b".to_string()]
        );
        let removed = parse("[[nodes]]\nnames = \"b\"\nweight = 2\n");
        assert_eq!(
            restart_only_node_changes(&old, &removed),
            vec!["a".to_string()]
        );
    }
}
