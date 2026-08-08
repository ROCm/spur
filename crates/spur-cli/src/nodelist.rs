// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use anyhow::{Context, Result};

/// Resolve the first allocated hostname from a controller-supplied `nodelist`.
///
/// The controller reports a job's nodes as a *compressed* Slurm-style hostlist
/// (e.g. `node[001-002]`), not a raw comma-separated list, so a plain
/// `split(',')` yields a bracket fragment rather than a hostname. Expanding the
/// pattern (the inverse of the controller's `compress`) gives the true first
/// node. Falls back to a comma-split if expansion fails, so a malformed pattern
/// still yields something connectable instead of no node at all.
pub fn first_allocated_node(nodelist: &str) -> Option<String> {
    if let Ok(Some(host)) = spur_core::hostlist::expand_first(nodelist) {
        return Some(host);
    }
    nodelist
        .split(',')
        .map(str::trim)
        .find(|name| !name.is_empty())
        .map(str::to_string)
}

pub fn resolve(nodelist: Option<String>, nodefile: Option<String>) -> Result<Option<String>> {
    if let Some(path) = nodefile {
        return read(&path).map(Some);
    }

    match nodelist {
        Some(value) if value.contains('/') => read(&value).map(Some),
        value => Ok(value),
    }
}

fn read(path: &str) -> Result<String> {
    let contents = std::fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read node list file: {}",
            Path::new(path).display()
        )
    })?;

    Ok(contents
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_file(contents: &str) -> (tempfile::TempDir, String) {
        let directory = tempfile::tempdir().expect("create fixture directory");
        let path = directory.path().join("nodes.txt");
        std::fs::write(&path, contents).expect("write node file fixture");
        (directory, path.to_string_lossy().into_owned())
    }

    #[test]
    fn first_allocated_node_expands_compressed_patterns() {
        assert_eq!(
            first_allocated_node("node[001-002]").as_deref(),
            Some("node001")
        );
        assert_eq!(
            first_allocated_node("node[001,003]").as_deref(),
            Some("node001")
        );
        assert_eq!(
            first_allocated_node("gpu[001-004],cpu[001-002]").as_deref(),
            Some("gpu001")
        );
        assert_eq!(
            first_allocated_node("node[9,010-011]").as_deref(),
            Some("node9")
        );
    }

    #[test]
    fn first_allocated_node_handles_plain_and_single() {
        assert_eq!(
            first_allocated_node("node001,node002").as_deref(),
            Some("node001")
        );
        assert_eq!(first_allocated_node("node007").as_deref(), Some("node007"));
    }

    #[test]
    fn first_allocated_node_empty_is_none() {
        assert_eq!(first_allocated_node(""), None);
    }

    #[test]
    fn first_allocated_node_falls_back_on_malformed_pattern() {
        assert_eq!(
            first_allocated_node("node[001-002").as_deref(),
            Some("node[001-002")
        );
    }

    #[test]
    fn leaves_literal_nodelist_unchanged() {
        let resolved = resolve(Some("node[001-004]".into()), None).expect("resolve nodelist");
        assert_eq!(resolved.as_deref(), Some("node[001-004]"));
    }

    #[test]
    fn reads_nodelist_value_containing_slash() {
        let (_directory, path) = node_file("node001\nnode002,node003\n");
        let resolved = resolve(Some(path), None).expect("resolve nodelist file");
        assert_eq!(resolved.as_deref(), Some("node001,node002,node003"));
    }

    #[test]
    fn explicit_nodefile_always_reads_file() {
        let (_directory, path) = node_file("node[001-003,007] node008\n");
        let resolved = resolve(None, Some(path));
        assert_eq!(
            resolved.expect("resolve nodefile").as_deref(),
            Some("node[001-003,007],node008")
        );
    }

    #[test]
    fn reports_nodefile_path_on_read_failure() {
        let error = resolve(None, Some("missing-nodes.txt".into())).expect_err("read must fail");
        assert!(error
            .to_string()
            .contains("failed to read node list file: missing-nodes.txt"));
    }
}
