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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct NodeFileFixture {
        directory: PathBuf,
        path: PathBuf,
    }

    impl NodeFileFixture {
        fn new(contents: &str) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "spur-nodefile-test-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).expect("create fixture directory");
            let path = directory.join("nodes.txt");
            std::fs::write(&path, contents).expect("write node file fixture");
            Self { directory, path }
        }

        fn path(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }
    }

    impl Drop for NodeFileFixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.directory).expect("remove fixture directory");
        }
    }

    #[test]
    fn leaves_literal_nodelist_unchanged() {
        let resolved = resolve(Some("node[001-004]".into()), None).expect("resolve nodelist");
        assert_eq!(resolved.as_deref(), Some("node[001-004]"));
    }

    #[test]
    fn reads_nodelist_value_containing_slash() {
        let fixture = NodeFileFixture::new("node001\nnode002,node003\n");
        let resolved = resolve(Some(fixture.path()), None).expect("resolve nodelist file");
        assert_eq!(resolved.as_deref(), Some("node001,node002,node003"));
    }

    #[test]
    fn explicit_nodefile_always_reads_file() {
        let fixture = NodeFileFixture::new("node[001-003,007] node008\n");
        let resolved = resolve(None, Some(fixture.path()));
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
