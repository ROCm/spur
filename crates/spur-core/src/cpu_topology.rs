// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Linux caps CPU IDs far below this; the bound keeps a typo such as
/// "0-4294967295" from allocating billions of entries.
const MAX_CPU_ID: u32 = 1 << 16;

/// One physical core: all of its online hyperthreads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreInfo {
    pub socket: u32,
    #[serde(default)]
    pub numa_node: Option<u32>,
    pub cpus: Vec<u32>,
}

/// Online CPUs grouped by physical core. A core's position is its abstract
/// core index: socket-major, then by lowest CPU ID, as in Slurm.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTopology {
    pub cores: Vec<CoreInfo>,
}

/// One online CPU as read from sysfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuRecord {
    pub cpu: u32,
    pub socket: u32,
    /// Linux `topology/core_id`, unique only within a socket.
    pub core_id: u32,
    pub numa_node: Option<u32>,
}

impl CpuTopology {
    pub fn from_cpus(records: &[CpuRecord]) -> CpuTopology {
        let mut by_core: BTreeMap<(u32, u32), CoreInfo> = BTreeMap::new();
        for r in records {
            by_core
                .entry((r.socket, r.core_id))
                .or_insert_with(|| CoreInfo {
                    socket: r.socket,
                    numa_node: r.numa_node,
                    cpus: Vec::new(),
                })
                .cpus
                .push(r.cpu);
        }
        let mut cores: Vec<CoreInfo> = by_core
            .into_values()
            .map(|mut core| {
                core.cpus.sort_unstable();
                core.cpus.dedup();
                core
            })
            .collect();
        cores.sort_by_key(|c| (c.socket, c.cpus.first().copied().unwrap_or(u32::MAX)));
        CpuTopology { cores }
    }

    pub fn online_cpus(&self) -> Vec<u32> {
        let mut cpus: Vec<u32> = self
            .cores
            .iter()
            .flat_map(|c| c.cpus.iter().copied())
            .collect();
        cpus.sort_unstable();
        cpus
    }

    /// Abstract index of the core that holds `cpu`.
    pub fn core_of_cpu(&self, cpu: u32) -> Option<usize> {
        self.cores.iter().position(|c| c.cpus.contains(&cpu))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CpuListError {
    #[error("invalid CPU list item {0:?}")]
    BadItem(String),
    #[error("CPU range {0}-{1} is reversed")]
    Reversed(u32, u32),
    #[error("CPU ID {0} is out of range")]
    TooLarge(u32),
}

/// Parse a Linux-style CPU list such as "0-3,8,10-11". Blank input is empty.
pub fn parse_cpu_list(s: &str) -> Result<Vec<u32>, CpuListError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let mut cpus = Vec::new();
    for item in s.split(',') {
        let item = item.trim();
        let parse = |t: &str| {
            let id = t
                .trim()
                .parse::<u32>()
                .map_err(|_| CpuListError::BadItem(item.to_string()))?;
            if id >= MAX_CPU_ID {
                return Err(CpuListError::TooLarge(id));
            }
            Ok(id)
        };
        match item.split_once('-') {
            Some((lo, hi)) => {
                let (lo, hi) = (parse(lo)?, parse(hi)?);
                if lo > hi {
                    return Err(CpuListError::Reversed(lo, hi));
                }
                cpus.extend(lo..=hi);
            }
            None => cpus.push(parse(item)?),
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    Ok(cpus)
}

/// Format CPUs as a compact sorted list such as "0-3,8".
pub fn format_cpu_list(cpus: &[u32]) -> String {
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        i += 1;
    }
    parts.join(",")
}

impl From<&CpuTopology> for spur_proto::proto::CpuTopology {
    fn from(t: &CpuTopology) -> Self {
        spur_proto::proto::CpuTopology {
            cores: t
                .cores
                .iter()
                .map(|c| spur_proto::proto::CoreInfo {
                    socket: c.socket,
                    numa_node: c.numa_node,
                    cpus: c.cpus.clone(),
                })
                .collect(),
        }
    }
}

impl From<spur_proto::proto::CpuTopology> for CpuTopology {
    fn from(t: spur_proto::proto::CpuTopology) -> Self {
        CpuTopology {
            cores: t
                .cores
                .into_iter()
                .map(|c| CoreInfo {
                    socket: c.socket,
                    numa_node: c.numa_node,
                    cpus: c.cpus,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(cpu: u32, socket: u32, core_id: u32) -> CpuRecord {
        CpuRecord {
            cpu,
            socket,
            core_id,
            numa_node: Some(socket),
        }
    }

    #[test]
    fn siblings_group_into_cores_in_socket_major_order() {
        // Siblings listed out of order and socket 1 first, as sysfs may.
        let records = [
            record(193, 1, 1),
            record(64, 1, 0),
            record(65, 1, 1),
            record(192, 1, 0),
            record(128, 0, 0),
            record(1, 0, 1),
            record(0, 0, 0),
            record(129, 0, 1),
        ];
        let topo = CpuTopology::from_cpus(&records);
        let cpus: Vec<Vec<u32>> = topo.cores.iter().map(|c| c.cpus.clone()).collect();
        assert_eq!(
            cpus,
            vec![vec![0, 128], vec![1, 129], vec![64, 192], vec![65, 193]]
        );
        assert_eq!(topo.cores[2].socket, 1);
        assert_eq!(topo.cores[2].numa_node, Some(1));
        assert_eq!(topo.core_of_cpu(193), Some(3));
        assert_eq!(topo.core_of_cpu(7), None);
        assert_eq!(topo.online_cpus(), vec![0, 1, 64, 65, 128, 129, 192, 193]);
    }

    #[test]
    fn cpu_lists_parse_and_format() {
        assert_eq!(
            parse_cpu_list("0-3,8, 10-11").unwrap(),
            vec![0, 1, 2, 3, 8, 10, 11]
        );
        assert_eq!(parse_cpu_list("  ").unwrap(), Vec::<u32>::new());
        assert_eq!(parse_cpu_list("5,5,4").unwrap(), vec![4, 5]);
        assert_eq!(format_cpu_list(&[8, 0, 1, 2, 3, 127, 255]), "0-3,8,127,255");
        assert_eq!(format_cpu_list(&[]), "");
    }

    #[test]
    fn bad_cpu_lists_are_rejected() {
        assert!(matches!(
            parse_cpu_list("abc"),
            Err(CpuListError::BadItem(_))
        ));
        assert!(matches!(
            parse_cpu_list("1,"),
            Err(CpuListError::BadItem(_))
        ));
        assert_eq!(parse_cpu_list("3-1"), Err(CpuListError::Reversed(3, 1)));
        assert_eq!(
            parse_cpu_list("0-4294967295"),
            Err(CpuListError::TooLarge(4_294_967_295))
        );
    }

    #[test]
    fn topology_survives_the_proto_round_trip() {
        let topo = CpuTopology {
            cores: vec![
                CoreInfo {
                    socket: 0,
                    numa_node: Some(0),
                    cpus: vec![0, 128],
                },
                CoreInfo {
                    socket: 1,
                    numa_node: None,
                    cpus: vec![64, 192],
                },
            ],
        };
        let wire = spur_proto::proto::CpuTopology::from(&topo);
        assert_eq!(CpuTopology::from(wire), topo);
    }
}
