// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use anyhow::{Context, Result};

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
