// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller policy for which nodes can apply a job's container mounts.

use spur_core::container_mount::wants_tmpfs;
use spur_core::job::JobSpec;
use spur_core::node::{Node, CAP_CONTAINER_TMPFS};

use crate::script_placement::PlacementRefusal;

/// Keeps a job that mounts a tmpfs off agents that would drop the entry and run
/// without it; retried once those agents are upgraded.
pub fn require_tmpfs_support(spec: &JobSpec, nodes: &[Node]) -> Result<(), PlacementRefusal> {
    if spec.container_image.as_deref().unwrap_or("").is_empty()
        || !wants_tmpfs(&spec.container_mounts)
    {
        return Ok(());
    }
    let outdated: Vec<&str> = nodes
        .iter()
        .filter(|node| !node.supports(CAP_CONTAINER_TMPFS))
        .map(|node| node.name.as_str())
        .collect();
    if outdated.is_empty() {
        return Ok(());
    }
    Err(PlacementRefusal::Backoff(format!(
        "a tmpfs container mount needs a newer spurd on {}",
        outdated.join(",")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(image: Option<&str>, mounts: &[&str]) -> JobSpec {
        JobSpec {
            container_image: image.map(str::to_string),
            container_mounts: mounts.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        }
    }

    fn node(name: &str, capable: bool) -> Node {
        let mut node = Node::new(name.into(), Default::default());
        if capable {
            node.capabilities = vec![CAP_CONTAINER_TMPFS.into()];
        }
        node
    }

    #[test]
    fn a_tmpfs_job_waits_for_every_node_to_be_upgraded() {
        let nodes = [node("n1", true), node("n2", false), node("n3", false)];
        let job = spec(Some("img.sqsh"), &["/a:/a,tmpfs:/cache:size=64M"]);

        assert_eq!(
            require_tmpfs_support(&job, &nodes),
            Err(PlacementRefusal::Backoff(
                "a tmpfs container mount needs a newer spurd on n2,n3".into()
            ))
        );
        assert_eq!(require_tmpfs_support(&job, &nodes[..1]), Ok(()));
    }

    #[test]
    fn jobs_without_a_tmpfs_mount_are_not_held_back() {
        let old = [node("n1", false)];

        assert_eq!(
            require_tmpfs_support(&spec(Some("img.sqsh"), &["/a:/a"]), &old),
            Ok(())
        );
        assert_eq!(
            require_tmpfs_support(&spec(Some("img.sqsh"), &[]), &old),
            Ok(())
        );
    }

    #[test]
    fn mounts_without_an_image_are_never_applied_so_they_never_gate() {
        let old = [node("n1", false)];

        assert_eq!(
            require_tmpfs_support(&spec(None, &["tmpfs:/cache"]), &old),
            Ok(())
        );
    }
}
