// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::topology::{
    DeviceTopology, FabricObservation, ObservationIssueKind as Issue, ObservationStatus as Status,
};

const SOURCE: &str = "amd-ualink";
const MAX_ATTRIBUTE_BYTES: u64 = 16 * 1024;

/// Reads only the observation ABI. Programming interfaces are never opened.
pub fn observe(device: &Path) -> Option<FabricObservation> {
    let root = device.join("ualink");
    match std::fs::metadata(&root) {
        Ok(meta) if meta.is_dir() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        _ => {
            return Some(FabricObservation::unavailable(
                SOURCE,
                "ualink",
                Issue::Unreadable,
            ))
        }
    }
    Some(stable_snapshot(|| read_snapshot(&root)))
}

fn stable_snapshot(mut read: impl FnMut() -> FabricObservation) -> FabricObservation {
    let first = read();
    let mut second = read();
    // Sysfs offers no atomic multi-file snapshot. Two equal reads reduce torn
    // observations; they do not prove that the fabric cannot change afterward.
    if first != second {
        second.status = Status::Inconsistent;
        second.add_issue("snapshot", Issue::Changed);
    }
    second
}

fn attribute(root: &Path, field: &str) -> Result<String, Issue> {
    let file = File::open(root.join(field)).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => Issue::Missing,
        _ => Issue::Unreadable,
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_ATTRIBUTE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Issue::Unreadable)?;
    if bytes.len() as u64 > MAX_ATTRIBUTE_BYTES {
        return Err(Issue::TooLarge);
    }
    String::from_utf8(bytes)
        .map(|value| value.trim().to_owned())
        .map_err(|_| Issue::Malformed)
}

fn read_field<T>(
    root: &Path,
    field: &str,
    observation: &mut FabricObservation,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Option<T> {
    match attribute(root, field).and_then(|value| parse(&value).ok_or(Issue::Malformed)) {
        Ok(value) => Some(value),
        Err(kind) => {
            observation.status = Status::Incomplete;
            observation.add_issue(field, kind);
            None
        }
    }
}

fn identifier(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 256
        && !value.chars().any(|c| c.is_whitespace() || c.is_control()))
    .then(|| value.to_owned())
}

fn endpoint(value: &str) -> Option<String> {
    value.parse::<u32>().ok().map(|id| id.to_string())
}

fn endpoints(value: &str) -> Option<Vec<String>> {
    let mut values = value
        .split_whitespace()
        .map(endpoint)
        .collect::<Option<Vec<_>>>()?;
    values.sort();
    values.dedup();
    Some(values)
}

fn read_snapshot(root: &Path) -> FabricObservation {
    let mut o = FabricObservation {
        source: SOURCE.into(),
        status: Status::Observed,
        ..Default::default()
    };
    o.physical_domain_id = read_field(root, "ppod_id", &mut o, identifier);
    o.physical_capacity = read_field(root, "ppod_size", &mut o, |s| s.parse().ok());
    o.fabric_domain_id = read_field(root, "vpod_id", &mut o, endpoint);
    o.fabric_capacity = read_field(root, "vpod_size", &mut o, |s| s.parse().ok());
    o.endpoint_id = read_field(root, "accel_id", &mut o, endpoint);
    o.endpoint_state = read_field(root, "accel_state", &mut o, identifier);
    o.transport = read_field(root, "link_type", &mut o, identifier);
    let active = read_field(root, "vpod_active_accels", &mut o, endpoints);
    let local = read_field(root, "local_accels", &mut o, endpoints);
    if let (Some(id), Some(ids)) = (&o.endpoint_id, &local) {
        if !ids.contains(id) {
            conflict(&mut o, "local_accels");
        }
    }
    if let (Some(id), Some(ids)) = (&o.endpoint_id, &active) {
        if o.endpoint_state.as_deref() == Some("active") && !ids.contains(id) {
            conflict(&mut o, "vpod_active_accels");
        }
    }
    if let (Some(capacity), Some(ids)) = (o.fabric_capacity, &active) {
        if ids.len() > capacity as usize {
            conflict(&mut o, "vpod_size");
        }
    }
    if let (Some(physical), Some(fabric)) = (o.physical_capacity, o.fabric_capacity) {
        if fabric > physical {
            conflict(&mut o, "ppod_size");
        }
    }
    o.active_endpoints = active.unwrap_or_default();
    o.local_endpoints = local.unwrap_or_default();
    o
}

fn conflict(o: &mut FabricObservation, field: &str) {
    o.status = Status::Inconsistent;
    o.add_issue(field, Issue::Conflict);
}

/// Cross-checks only facts scoped to the same physical domain on this node.
pub fn validate_local(topologies: &mut [&mut DeviceTopology]) {
    let mut conflicts = vec![Vec::new(); topologies.len()];
    for (i, left) in topologies.iter().enumerate() {
        let Some(a) = &left.fabric else { continue };
        for (j, right) in topologies.iter().enumerate().skip(i + 1) {
            let Some(b) = &right.fabric else { continue };
            if a.source != SOURCE
                || b.source != SOURCE
                || a.physical_domain_id.is_none()
                || a.physical_domain_id != b.physical_domain_id
            {
                continue;
            }
            let mut fields = Vec::new();
            if a.physical_capacity.is_some()
                && b.physical_capacity.is_some()
                && a.physical_capacity != b.physical_capacity
            {
                fields.push("ppod_size");
            }
            if !has_issue(a, "local_accels")
                && !has_issue(b, "local_accels")
                && a.local_endpoints != b.local_endpoints
            {
                fields.push("local_accels");
            }
            if a.fabric_domain_id.is_some() && a.fabric_domain_id == b.fabric_domain_id {
                if a.fabric_capacity.is_some()
                    && b.fabric_capacity.is_some()
                    && a.fabric_capacity != b.fabric_capacity
                {
                    fields.push("vpod_size");
                }
                if !has_issue(a, "vpod_active_accels")
                    && !has_issue(b, "vpod_active_accels")
                    && a.active_endpoints != b.active_endpoints
                {
                    fields.push("vpod_active_accels");
                }
            }
            // Logical partitions can share an endpoint only on the same BDF.
            if a.endpoint_id.is_some()
                && a.endpoint_id == b.endpoint_id
                && left.pci_bdf != right.pci_bdf
            {
                fields.push("accel_id");
            }
            conflicts[i].extend(fields.iter().copied());
            conflicts[j].extend(fields);
        }
    }
    for (topology, fields) in topologies.iter_mut().zip(conflicts) {
        if let Some(o) = &mut topology.fabric {
            for field in fields {
                conflict(o, field);
            }
        }
    }
}

fn has_issue(o: &FabricObservation, field: &str) -> bool {
    o.issues
        .iter()
        .any(|issue| issue.field == field && issue.kind != Issue::Conflict)
}

pub fn pci_root(device: &Path) -> Option<String> {
    let resolved = device.canonicalize().ok()?;
    resolved
        .ancestors()
        .filter_map(|path| path.file_name()?.to_str())
        .find(|name| {
            let bytes = name.as_bytes();
            bytes.len() == 10
                && name.starts_with("pci")
                && bytes[7] == b':'
                && bytes[3..7]
                    .iter()
                    .chain(&bytes[8..])
                    .all(u8::is_ascii_hexdigit)
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ualink");
        std::fs::create_dir(&root).unwrap();
        for (name, value) in [
            ("ppod_id", "00000000-0000-4000-8000-000000000001"),
            ("ppod_size", "72"),
            ("vpod_id", "15"),
            ("vpod_size", "4"),
            ("vpod_active_accels", "60 61 62 63"),
            ("local_accels", "63 62 61 60"),
            ("accel_id", "63"),
            ("accel_state", "active"),
            ("link_type", "UALoE"),
        ] {
            std::fs::write(root.join(name), format!("{value}\n")).unwrap();
        }
        dir
    }

    #[test]
    fn observes_normalized_membership_without_reading_programming_interfaces() {
        let dir = fixture();
        // A directory cannot be parsed as an attribute; an accidental read would fail.
        std::fs::create_dir(dir.path().join("ualink/config")).unwrap();
        let o = observe(dir.path()).unwrap();
        assert_eq!(o.status, Status::Observed);
        assert_eq!(o.fabric_domain_id.as_deref(), Some("15"));
        assert_eq!(o.active_endpoints, o.local_endpoints);
        assert_eq!(o.transport.as_deref(), Some("UALoE"));
        assert!(o.issues.is_empty());
        std::fs::write(
            dir.path().join("ualink/vpod_active_accels"),
            "063 61 60 62 61",
        )
        .unwrap();
        assert_eq!(observe(dir.path()).unwrap(), o);
    }

    #[test]
    fn absent_abi_is_distinct_from_missing_or_malformed_attributes() {
        let empty = tempfile::tempdir().unwrap();
        assert!(observe(empty.path()).is_none());
        let dir = fixture();
        std::fs::remove_file(dir.path().join("ualink/vpod_id")).unwrap();
        let o = observe(dir.path()).unwrap();
        assert_eq!(o.status, Status::Incomplete);
        assert!(o.fabric_domain_id.is_none());
        assert!(o
            .issues
            .iter()
            .any(|i| i.field == "vpod_id" && i.kind == Issue::Missing));
        std::fs::write(dir.path().join("ualink/vpod_id"), "unknown").unwrap();
        assert!(observe(dir.path())
            .unwrap()
            .issues
            .iter()
            .any(|i| i.field == "vpod_id" && i.kind == Issue::Malformed));
    }

    #[test]
    fn bounds_attribute_reads_and_preserves_zero_identity() {
        let dir = fixture();
        std::fs::write(dir.path().join("ualink/vpod_id"), "0").unwrap();
        assert_eq!(
            observe(dir.path()).unwrap().fabric_domain_id.as_deref(),
            Some("0")
        );
        std::fs::write(
            dir.path().join("ualink/local_accels"),
            vec![b'1'; MAX_ATTRIBUTE_BYTES as usize + 1],
        )
        .unwrap();
        let o = observe(dir.path()).unwrap();
        assert_eq!(o.status, Status::Incomplete);
        assert!(o
            .issues
            .iter()
            .any(|i| i.field == "local_accels" && i.kind == Issue::TooLarge));
        std::fs::write(dir.path().join("ualink/accel_state"), [0xff]).unwrap();
        assert!(observe(dir.path())
            .unwrap()
            .issues
            .iter()
            .any(|i| i.field == "accel_state" && i.kind == Issue::Malformed));
    }

    #[test]
    fn changed_read_pair_cannot_be_observed() {
        let dir = fixture();
        let mut reads = 0;
        let o = stable_snapshot(|| {
            reads += 1;
            if reads == 2 {
                std::fs::write(dir.path().join("ualink/vpod_id"), "16").unwrap();
            }
            read_snapshot(&dir.path().join("ualink"))
        });
        assert_eq!(o.status, Status::Inconsistent);
        assert!(o
            .issues
            .iter()
            .any(|i| i.field == "snapshot" && i.kind == Issue::Changed));
    }

    #[test]
    fn local_disagreement_marks_both_views_but_domains_are_scoped() {
        let a = fixture();
        let b = fixture();
        std::fs::write(b.path().join("ualink/accel_id"), "62").unwrap();
        std::fs::write(b.path().join("ualink/vpod_active_accels"), "60 61 62").unwrap();
        let mut left = DeviceTopology {
            pci_bdf: Some("0001:04:00.0".into()),
            fabric: observe(a.path()),
            ..Default::default()
        };
        let mut right = DeviceTopology {
            pci_bdf: Some("0002:04:00.0".into()),
            fabric: observe(b.path()),
            ..Default::default()
        };
        validate_local(&mut [&mut left, &mut right]);
        assert_eq!(left.fabric.as_ref().unwrap().status, Status::Inconsistent);
        assert_eq!(right.fabric.as_ref().unwrap().status, Status::Inconsistent);
        std::fs::write(
            b.path().join("ualink/ppod_id"),
            "00000000-0000-4000-8000-000000000002",
        )
        .unwrap();
        left.fabric = observe(a.path());
        right.fabric = observe(b.path());
        validate_local(&mut [&mut left, &mut right]);
        assert_eq!(left.fabric.unwrap().status, Status::Observed);
        assert_eq!(right.fabric.unwrap().status, Status::Observed);
    }

    #[test]
    fn logical_partitions_may_share_one_physical_endpoint() {
        let dir = fixture();
        let mut a = DeviceTopology {
            pci_bdf: Some("0001:04:00.0".into()),
            fabric: observe(dir.path()),
            ..Default::default()
        };
        let mut b = a.clone();
        validate_local(&mut [&mut a, &mut b]);
        assert_eq!(a.fabric.as_ref().unwrap().status, Status::Observed);
        b.pci_bdf = Some("0002:04:00.0".into());
        validate_local(&mut [&mut a, &mut b]);
        assert!(a
            .fabric
            .unwrap()
            .issues
            .iter()
            .any(|i| i.field == "accel_id"));
    }

    #[test]
    fn resolves_node_scoped_pci_host_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir
            .path()
            .join("devices/pci0001:00/0001:00:01.1/0001:04:00.0");
        std::fs::create_dir_all(&device).unwrap();
        assert_eq!(pci_root(&device).as_deref(), Some("pci0001:00"));
        assert_eq!(pci_root(dir.path()), None);
    }
}
