// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Links that let a DRA kubelet plugin built for `/var/lib/kubelet` register
//! with the k0s kubelet, whose root is `/var/lib/k0s/kubelet`. The k0s kubelet
//! keeps a real `/var/lib/kubelet/device-plugins`, so only the two DRA
//! subdirectories are linked, never the whole directory.

use std::io;
use std::path::{Path, PathBuf};

const SUBDIRS: [&str; 2] = ["plugins_registry", "plugins"];

#[derive(Debug, Clone)]
pub struct KubeletLinks {
    kubelet_dir: PathBuf,
    k0s_kubelet_dir: PathBuf,
}

impl KubeletLinks {
    pub fn system() -> Self {
        Self::under(Path::new("/"))
    }

    /// Links below `root` instead of `/`.
    pub fn under(root: &Path) -> Self {
        Self {
            kubelet_dir: root.join("var/lib/kubelet"),
            k0s_kubelet_dir: root.join("var/lib/k0s/kubelet"),
        }
    }

    fn pairs(&self) -> impl Iterator<Item = (PathBuf, PathBuf)> + '_ {
        SUBDIRS
            .iter()
            .map(|s| (self.kubelet_dir.join(s), self.k0s_kubelet_dir.join(s)))
    }

    /// Create the links. A path that already holds anything but our link is
    /// left alone, and its presence is returned as the unshareable reason.
    pub fn ensure(&self) -> Result<(), String> {
        let mut missing = Vec::new();
        for (link, target) in self.pairs() {
            match std::fs::symlink_metadata(&link) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push((link, target)),
                Err(e) => return Err(format!("cannot inspect {}: {e}", link.display())),
                Ok(_) if is_link_to(&link, &target) => {}
                Ok(_) => {
                    return Err(format!(
                        "{} exists and is not a link to {}",
                        link.display(),
                        target.display()
                    ))
                }
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.kubelet_dir)
            .map_err(|e| format!("cannot create {}: {e}", self.kubelet_dir.display()))?;
        for (link, target) in missing {
            std::os::unix::fs::symlink(&target, &link)
                .map_err(|e| format!("cannot link {}: {e}", link.display()))?;
        }
        Ok(())
    }

    /// Whether any of our links is present, i.e. the node was shared.
    pub fn any_ours(&self) -> bool {
        self.pairs()
            .any(|(link, target)| is_link_to(&link, &target))
    }

    /// Remove our links; anything else at those paths, and the directory, stay.
    pub fn remove_ours(&self) -> io::Result<()> {
        for (link, target) in self.pairs() {
            if is_link_to(&link, &target) {
                std::fs::remove_file(&link)?;
            }
        }
        Ok(())
    }
}

fn is_link_to(link: &Path, target: &Path) -> bool {
    std::fs::read_link(link).is_ok_and(|t| t == target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links() -> (tempfile::TempDir, KubeletLinks) {
        let root = tempfile::tempdir().unwrap();
        let links = KubeletLinks::under(root.path());
        (root, links)
    }

    fn kubelet(root: &Path, sub: &str) -> PathBuf {
        root.join("var/lib/kubelet").join(sub)
    }

    #[test]
    fn creates_both_links_when_absent() {
        let (root, links) = links();

        links.ensure().unwrap();

        for sub in SUBDIRS {
            assert_eq!(
                std::fs::read_link(kubelet(root.path(), sub)).unwrap(),
                root.path().join("var/lib/k0s/kubelet").join(sub)
            );
        }
        assert!(links.any_ours());
    }

    #[test]
    fn own_links_are_accepted_again() {
        let (_root, links) = links();
        links.ensure().unwrap();

        links.ensure().unwrap();
    }

    #[test]
    fn foreign_directory_is_left_and_reported() {
        let (root, links) = links();
        std::fs::create_dir_all(kubelet(root.path(), "plugins")).unwrap();

        let reason = links.ensure().unwrap_err();

        assert!(reason.contains("var/lib/kubelet/plugins exists and is not a link to"));
        assert!(kubelet(root.path(), "plugins").is_dir());
        assert!(!kubelet(root.path(), "plugins_registry").exists());
    }

    #[test]
    fn foreign_link_is_left_and_reported() {
        let (root, links) = links();
        std::fs::create_dir_all(root.path().join("var/lib/kubelet")).unwrap();
        std::os::unix::fs::symlink("/elsewhere", kubelet(root.path(), "plugins_registry")).unwrap();

        let reason = links.ensure().unwrap_err();

        assert!(reason.contains("plugins_registry exists and is not a link to"));
        assert_eq!(
            std::fs::read_link(kubelet(root.path(), "plugins_registry")).unwrap(),
            PathBuf::from("/elsewhere")
        );
        assert!(!links.any_ours());
    }

    #[test]
    fn removal_keeps_foreign_paths_and_the_directory() {
        let (root, links) = links();
        links.ensure().unwrap();
        std::fs::remove_file(kubelet(root.path(), "plugins")).unwrap();
        std::fs::create_dir(kubelet(root.path(), "plugins")).unwrap();

        links.remove_ours().unwrap();

        assert!(std::fs::symlink_metadata(kubelet(root.path(), "plugins_registry")).is_err());
        assert!(kubelet(root.path(), "plugins").is_dir());
        assert!(root.path().join("var/lib/kubelet").is_dir());
    }
}
