// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Small readers over sysfs and /dev shared by every device discovery.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::cdi::spec::DeviceNode;

/// `host_path` as seen under `root`, so discovery can run against a fixture tree.
pub(crate) fn under_root(root: &Path, host_path: &str) -> PathBuf {
    root.join(host_path.trim_start_matches('/'))
}

pub(crate) fn read_sysfs_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// sysfs reports -1 for a device with no NUMA affinity.
pub(crate) fn read_numa_node(device_path: &Path) -> Option<u32> {
    read_sysfs_string(&device_path.join("numa_node")).and_then(|s| {
        let val: i32 = s.parse().ok()?;
        u32::try_from(val).ok()
    })
}

/// A `key value` per line file, as KFD topology writes them. A malformed line
/// is skipped: dropping the whole file would hide an otherwise healthy device.
pub(crate) fn parse_kv_file(path: &Path) -> Option<HashMap<String, String>> {
    let content = std::fs::read_to_string(path).ok()?;
    let map = content
        .lines()
        .filter_map(|line| line.trim().split_once(' '))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    Some(map)
}

#[cfg(unix)]
struct DeviceStat {
    major: i64,
    minor: i64,
    file_mode: u32,
    uid: u32,
    gid: u32,
}

#[cfg(unix)]
fn stat_device_node(path: &Path) -> Option<DeviceStat> {
    let meta = std::fs::metadata(path).ok()?;
    let rdev = meta.rdev();
    if rdev == 0 {
        return None;
    }
    let major = (((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)) as i64;
    let minor = ((rdev & 0xff) | ((rdev >> 12) & !0xff)) as i64;
    Some(DeviceStat {
        major,
        minor,
        file_mode: meta.mode() & 0o7777,
        uid: meta.uid(),
        gid: meta.gid(),
    })
}

/// A character device node with the host's major/minor and ownership, when it exists.
pub(crate) fn char_device_node(root: &Path, path: String) -> DeviceNode {
    let stat = stat_device_node(&under_root(root, &path));
    DeviceNode {
        path,
        host_path: None,
        r#type: Some("c".into()),
        major: stat.as_ref().map(|s| s.major),
        minor: stat.as_ref().map(|s| s.minor),
        file_mode: stat.as_ref().map(|s| s.file_mode),
        permissions: Some("rwm".into()),
        uid: stat.as_ref().map(|s| s.uid),
        gid: stat.as_ref().map(|s| s.gid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_line_does_not_discard_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("properties");
        std::fs::write(&path, "simd_count 304\nnoseparator\nvendor_id 4098\n").unwrap();

        let props = parse_kv_file(&path).expect("a readable file parses");
        assert_eq!(props.get("simd_count").map(String::as_str), Some("304"));
        assert_eq!(props.get("vendor_id").map(String::as_str), Some("4098"));
    }

    #[test]
    fn a_negative_numa_node_means_no_affinity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("numa_node"), "-1\n").unwrap();
        assert_eq!(read_numa_node(dir.path()), None);
        std::fs::write(dir.path().join("numa_node"), "1\n").unwrap();
        assert_eq!(read_numa_node(dir.path()), Some(1));
    }
}
