// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::job::JobSpec;
use crate::resource::GpuResource;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GpuField {
    Type,
    Vram,
    PhysicalPod,
    Vpod,
    Numa,
    PciRoot,
}

impl GpuField {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "gpu.type" | "gpu.gpu_type" => Ok(Self::Type),
            "gpu.vram" | "gpu.vram_bytes" => Ok(Self::Vram),
            "gpu.physical_pod_id" | "gpu.physical_domain_id" => Ok(Self::PhysicalPod),
            "gpu.vpod_id" | "gpu.fabric_domain_id" => Ok(Self::Vpod),
            "gpu.numa_node_id" => Ok(Self::Numa),
            "gpu.pcie_root_id" => Ok(Self::PciRoot),
            _ => Err(format!("unsupported GPU constraint field {value:?}")),
        }
    }

    fn numeric(self) -> bool {
        matches!(self, Self::Vram | Self::Numa)
    }

    fn value(self, gpu: &GpuResource) -> Option<Value> {
        if self == Self::Type {
            return (!gpu.gpu_type.is_empty()).then(|| Value::Text(gpu.gpu_type.clone()));
        }
        let t = gpu.topology.as_ref()?;
        match self {
            Self::Vram => t.vram_bytes.map(Value::Number),
            Self::Numa => t.numa_node.map(|id| Value::Number(u64::from(id))),
            Self::PciRoot => t
                .pci_root
                .clone()
                .filter(|s| !s.is_empty())
                .map(Value::Text),
            Self::PhysicalPod | Self::Vpod => {
                let f = t.fabric.as_ref()?;
                if f.status != spur_devices::topology::ObservationStatus::Observed
                    || !f.issues.is_empty()
                {
                    return None;
                }
                let id = if self == Self::PhysicalPod {
                    &f.physical_domain_id
                } else {
                    &f.fabric_domain_id
                };
                id.clone().filter(|s| !s.is_empty()).map(Value::Text)
            }
            Self::Type => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Value {
    Number(u64),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Predicate {
    field: GpuField,
    comparison: Comparison,
    value: Value,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Constraints<'a> {
    pub features: Vec<&'a str>,
    pub same_fabric: bool,
    pub same: BTreeSet<GpuField>,
    pub predicates: Vec<Predicate>,
}

impl<'a> Constraints<'a> {
    pub fn parse(value: Option<&'a str>) -> Result<Self, String> {
        let value = value.unwrap_or_default();
        let typed = value.contains("same(")
            || (value.contains("gpu.") && value.contains(['(', ')', '=', '<', '>', '!']));
        if !typed {
            return Ok(Self {
                features: value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect(),
                ..Default::default()
            });
        }
        if value.len() > 4096 || value.contains('|') {
            return Err("unsupported GPU constraint: use at most 4096 bytes of predicates joined with ',' or '&'; OR is unsupported".into());
        }
        let mut result = Self::default();
        for (index, term) in value.split([',', '&']).map(str::trim).enumerate() {
            if index >= 32 {
                return Err("unsupported GPU constraint: at most 32 terms are allowed".into());
            }
            if let Some(field) = term.strip_prefix("same(").and_then(|s| s.strip_suffix(')')) {
                let field = GpuField::parse(field.trim())?;
                if field == GpuField::Vpod {
                    result.same_fabric = true;
                } else {
                    result.same.insert(field);
                }
                continue;
            }
            if term.starts_with("gpu.") {
                let at = term.find(['=', '!', '<', '>']).ok_or_else(|| {
                    format!("unsupported GPU constraint {term:?}: comparison required")
                })?;
                let field = GpuField::parse(term[..at].trim())?;
                let tail = &term[at..];
                let (operator, comparison) = [
                    ("!=", Comparison::Ne),
                    (">=", Comparison::Ge),
                    ("<=", Comparison::Le),
                    ("=", Comparison::Eq),
                    (">", Comparison::Gt),
                    ("<", Comparison::Lt),
                ]
                .into_iter()
                .find(|(op, _)| tail.starts_with(op))
                .ok_or_else(|| format!("unsupported GPU constraint operator {term:?}"))?;
                let raw = tail[operator.len()..].trim();
                if raw.is_empty()
                    || raw.contains(['(', ')', '=', '!', '<', '>', '"', '\''])
                    || raw.chars().any(char::is_whitespace)
                {
                    return Err(format!("unsupported GPU constraint value {raw:?}"));
                }
                let value = if field.numeric() {
                    Value::Number(parse_number(raw, field == GpuField::Vram)?)
                } else {
                    if !matches!(comparison, Comparison::Eq | Comparison::Ne) {
                        return Err(
                            "unsupported GPU constraint: string properties support only = and !="
                                .into(),
                        );
                    }
                    Value::Text(raw.into())
                };
                if field == GpuField::Vpod && comparison == Comparison::Eq {
                    result.same_fabric = true;
                }
                result.predicates.push(Predicate {
                    field,
                    comparison,
                    value,
                });
                continue;
            }
            if term.is_empty() || term.contains(['(', ')', '=', '!', '<', '>']) {
                return Err(format!("unsupported GPU constraint {term:?}"));
            }
            result.features.push(term);
        }
        Ok(result)
    }

    pub fn has_gpu(&self) -> bool {
        self.same_fabric || !self.same.is_empty() || !self.predicates.is_empty()
    }

    pub fn matches_gpu(&self, gpu: &GpuResource) -> bool {
        // Retained accounting identities for lost GPUs are never candidates.
        if gpu
            .topology
            .as_ref()
            .and_then(|t| t.fabric.as_ref())
            .is_some_and(|f| f.status == spur_devices::topology::ObservationStatus::Unavailable)
        {
            return false;
        }
        self.predicates.iter().all(|p| {
            let Some(value) = p.field.value(gpu) else {
                return false;
            };
            match p.comparison {
                Comparison::Eq => value == p.value,
                Comparison::Ne => value != p.value,
                Comparison::Gt => value > p.value,
                Comparison::Ge => value >= p.value,
                Comparison::Lt => value < p.value,
                Comparison::Le => value <= p.value,
            }
        })
    }

    pub fn group_key(&self, node: &str, gpu: &GpuResource) -> Option<Vec<String>> {
        let mut key = Vec::new();
        for field in &self.same {
            let value = field.value(gpu)?;
            if matches!(field, GpuField::Numa | GpuField::PciRoot) {
                key.push(node.into());
            }
            key.push(match value {
                Value::Number(n) => n.to_string(),
                Value::Text(s) => s,
            });
        }
        Some(key)
    }
}

fn parse_number(raw: &str, bytes: bool) -> Result<u64, String> {
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let number = raw[..split]
        .parse::<u64>()
        .map_err(|_| format!("invalid GPU property number {raw:?}"))?;
    let unit = raw[split..].to_ascii_lowercase();
    let multiplier: u64 = match (bytes, unit.as_str()) {
        (_, "") | (true, "b") => 1,
        (true, "kib") => 1024,
        (true, "mib") => 1024 * 1024,
        (true, "gib") => 1024 * 1024 * 1024,
        (true, "tib") => 1024_u64.pow(4),
        (true, "kb") => 1000,
        (true, "mb") => 1000 * 1000,
        (true, "gb") => 1000 * 1000 * 1000,
        (true, "tb") => 1000_u64.pow(4),
        _ => return Err(format!("invalid GPU property unit {raw:?}")),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("GPU property number overflows {raw:?}"))
}

pub fn requires_same_fabric(spec: &JobSpec) -> bool {
    Constraints::parse(spec.constraint.as_deref()).is_ok_and(|c| c.same_fabric)
}

pub fn requires_gpu_properties(spec: &JobSpec) -> bool {
    Constraints::parse(spec.constraint.as_deref()).is_ok_and(|c| c.has_gpu())
}

pub fn validate(spec: &JobSpec) -> Result<(), String> {
    let constraints = Constraints::parse(spec.constraint.as_deref())?;
    if constraints.has_gpu()
        && crate::gpu_request::resolve_gpu_demand(spec)
            .map_err(|e| e.to_string())?
            .is_none()
        && !spec.exclusive
    {
        return Err(
            "GPU property constraint requires a GPU request or whole-node exclusivity".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fabric_predicates_are_separate_from_node_features() {
        for name in ["vpod_id", "fabric_domain_id"] {
            let value = format!("fast&same(gpu.{name}),large");
            let parsed = Constraints::parse(Some(&value)).unwrap();
            assert_eq!(parsed.features, ["fast", "large"]);
            assert!(parsed.same_fabric);
        }
        assert_eq!(
            Constraints::parse(Some("a,, b")).unwrap().features,
            ["a", "b"]
        );
        assert_eq!(Constraints::parse(Some("a&b")).unwrap().features, ["a&b"]);
        assert_eq!(
            Constraints::parse(Some("gpu.fast")).unwrap().features,
            ["gpu.fast"]
        );
    }

    #[test]
    fn unsupported_expressions_never_drop_a_requirement() {
        for value in [
            "same(gpu.vpod_id)|fast",
            "same(gpu.vpod_id)&",
            "same(gpu.unknown)",
            "same (gpu.vpod_id)",
            "same(gpu.vpod_id)=false",
            "gpu.vram=18446744073709551615GiB",
            "gpu.numa_node_id=-1",
            "gpu.type>mi300x",
            "gpu.vram=1GiBX",
            "gpu.vram==1",
            "gpu.type=",
            "gpu.unknown=1",
        ] {
            assert!(Constraints::parse(Some(value)).is_err(), "{value}");
        }
        assert!(Constraints::parse(Some(&["gpu.type=a"; 33].join("&"))).is_err());
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use crate::resource::{DeviceTopology, GpuLinkType};
    use spur_devices::topology::{FabricObservation, ObservationStatus};

    fn gpu() -> GpuResource {
        GpuResource {
            device_id: 0,
            stable_id: 128,
            gpu_type: "mi300x".into(),
            memory_mb: 196608,
            peer_gpus: vec![],
            link_type: GpuLinkType::PCIe,
            topology: Some(DeviceTopology {
                pci_bdf: Some("0001:04:00.0".into()),
                pci_root: Some("pci0001:00".into()),
                numa_node: Some(0),
                vram_bytes: Some(192 * 1024_u64.pow(3)),
                fabric: Some(FabricObservation {
                    status: ObservationStatus::Observed,
                    physical_domain_id: Some("rack-a".into()),
                    fabric_domain_id: Some("15".into()),
                    ..Default::default()
                }),
            }),
        }
    }

    #[test]
    fn every_property_filters_each_selected_device() {
        for (expression, expected) in [
            ("gpu.vram=192GiB", true),
            ("gpu.vram!=192GiB", false),
            ("gpu.vram>192GiB", false),
            ("gpu.vram>=192GiB", true),
            ("gpu.vram<192GiB", false),
            ("gpu.vram<=192GiB", true),
            ("gpu.vram>=192GB", true),
            ("gpu.vram=196608MiB", true),
            ("gpu.vram_bytes=206158430208", true),
            ("gpu.type=mi300x&gpu.vram>=192GiB", true),
            ("gpu.type!=mi300x", false),
            ("gpu.physical_pod_id=rack-a", true),
            ("gpu.physical_pod_id!=rack-a", false),
            ("gpu.vpod_id=15", true),
            ("gpu.vpod_id!=15", false),
            ("gpu.numa_node_id=0", true),
            ("gpu.numa_node_id!=0", false),
            ("gpu.numa_node_id<1", true),
            ("gpu.numa_node_id>=1", false),
            ("gpu.pcie_root_id=pci0001:00", true),
            ("gpu.pcie_root_id!=pci0001:00", false),
        ] {
            let parsed = Constraints::parse(Some(expression)).unwrap();
            assert_eq!(parsed.matches_gpu(&gpu()), expected, "{expression}");
        }
    }

    #[test]
    fn missing_facts_do_not_satisfy_inequality_or_grouping() {
        let mut gpu = gpu();
        gpu.topology = None;
        for expression in [
            "gpu.vram!=0",
            "gpu.numa_node_id!=0",
            "gpu.pcie_root_id!=other",
            "gpu.physical_pod_id!=other",
            "gpu.vpod_id!=other",
        ] {
            assert!(
                !Constraints::parse(Some(expression))
                    .unwrap()
                    .matches_gpu(&gpu),
                "{expression}"
            );
        }
        for field in ["numa_node_id", "pcie_root_id", "physical_pod_id", "vram"] {
            let expr = format!("same(gpu.{field})");
            assert!(Constraints::parse(Some(&expr))
                .unwrap()
                .group_key("n0", &gpu)
                .is_none());
        }
    }

    #[test]
    fn root_and_numa_groups_are_scoped_by_node() {
        for expr in ["same(gpu.numa_node_id)", "same(gpu.pcie_root_id)"] {
            let parsed = Constraints::parse(Some(expr)).unwrap();
            assert_ne!(
                parsed.group_key("n0", &gpu()),
                parsed.group_key("n1", &gpu())
            );
            assert_eq!(
                parsed.group_key("n0", &gpu()),
                parsed.group_key("n0", &gpu())
            );
        }
        let parsed = Constraints::parse(Some("same(gpu.physical_pod_id)")).unwrap();
        assert_eq!(
            parsed.group_key("n0", &gpu()),
            parsed.group_key("n1", &gpu())
        );
    }
}
