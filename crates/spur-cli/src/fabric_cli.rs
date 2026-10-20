// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use spur_core::fabric::{FabricManifest, FabricView};

#[derive(Debug, Subcommand)]
pub enum TopologyCommand {
    /// Show explicit fabric blocks and membership diagnostics
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Print a versioned JSON membership document to stdout
    Export,
    /// Check a JSON membership document against current controller inventory
    Validate { file: PathBuf },
}

const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

fn read_manifest(path: &Path) -> Result<FabricManifest> {
    let file = std::fs::File::open(path).context("cannot open topology manifest")?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("topology manifest exceeds 8 MiB");
    }
    let manifest: FabricManifest =
        serde_json::from_slice(&bytes).context("invalid topology manifest")?;
    manifest.validate_shape().map_err(anyhow::Error::msg)?;
    Ok(manifest)
}

pub async fn run(controller: &str, command: TopologyCommand) -> Result<()> {
    let manifest = match &command {
        TopologyCommand::Validate { file } => Some(read_manifest(file)?),
        _ => None,
    };
    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);
    let mut request = tonic::Request::new(());
    request.set_timeout(std::time::Duration::from_secs(30));
    let response = client
        .get_fabric_topology(request)
        .await
        .context("failed to get fabric topology (requires controller support)")?;
    let view = FabricView::try_from(response.into_inner()).map_err(anyhow::Error::msg)?;
    match command {
        TopologyCommand::Show { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
            } else {
                print!("{}", format_view(&view));
            }
        }
        TopologyCommand::Export => {
            if !view.unresolved.is_empty() {
                bail!("some GPUs have unresolved membership; inspect topology show --json");
            }
            let manifest = FabricManifest::from_view(&view);
            manifest.resolve(&view).map_err(anyhow::Error::msg)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        TopologyCommand::Validate { .. } => {
            let manifest = manifest.context("missing topology manifest")?;
            let blocks = manifest.resolve(&view).map_err(anyhow::Error::msg)?;
            println!("Valid: {} blocks match current registered membership. This does not verify fabric health or reserve devices.", blocks.len());
        }
    }
    Ok(())
}

fn format_view(view: &FabricView) -> String {
    use std::fmt::Write;
    let mut out = format!(
        "Namespace={} Blocks={} UnresolvedGPUs={}\n",
        view.namespace,
        view.blocks.len(),
        view.unresolved.len()
    );
    for block in &view.blocks {
        let _ = writeln!(
            out,
            "PhysicalDomain={} FabricDomain={} LogicalGPUs={} Endpoints={} Membership={}",
            block.key.physical_domain,
            block.key.fabric_domain,
            block.members.len(),
            block.active_endpoints.len(),
            if block.issues.is_empty() {
                "consistent"
            } else {
                "invalid"
            }
        );
        let _ = writeln!(
            out,
            "   Issues={}",
            if block.issues.is_empty() {
                "none".into()
            } else {
                block.issues.join(",")
            }
        );
        for member in &block.members {
            let _ = writeln!(
                out,
                "   Node={} StableId={} Endpoint={} InventoryGeneration={}",
                member.device.node,
                member.device.stable_id,
                member.endpoint,
                member.inventory_generation
            );
        }
    }
    for item in &view.unresolved {
        let _ = writeln!(
            out,
            "UnresolvedNode={} StableId={} Issue={}",
            item.device.node, item.device.stable_id, item.issue
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use spur_core::fabric::{FabricBlock, FabricBlockKey, FabricDeviceKey, FabricMember};

    #[test]
    fn parses_topology_commands() {
        use crate::scontrol::{ScontrolArgs, ScontrolCommand};
        for name in ["scontrol", "control"] {
            let parsed =
                ScontrolArgs::try_parse_from([name, "topology", "show", "--json"]).unwrap();
            assert!(matches!(
                parsed.command,
                ScontrolCommand::Topology {
                    command: TopologyCommand::Show { json: true }
                }
            ));
            assert!(ScontrolArgs::try_parse_from([name, "topology", "validate"]).is_err());
        }
    }

    #[test]
    fn formats_scoped_members_and_failures() {
        let view = FabricView {
            namespace: "test".into(),
            unresolved: Vec::new(),
            blocks: vec![FabricBlock {
                key: FabricBlockKey {
                    namespace: "test".into(),
                    physical_domain: "rack-a".into(),
                    fabric_domain: "7".into(),
                },
                members: vec![FabricMember {
                    device: FabricDeviceKey {
                        node: "n1".into(),
                        stable_id: 42,
                    },
                    endpoint: "1".into(),
                    inventory_generation: 3,
                }],
                active_endpoints: vec!["1".into()],
                physical_capacity: Some(8),
                fabric_capacity: Some(1),
                issues: vec!["stale_node".into()],
            }],
        };
        let output = format_view(&view);
        assert!(output.contains("PhysicalDomain=rack-a FabricDomain=7"));
        assert!(output.contains("Membership=invalid"));
        assert!(output.contains("Issues=stale_node"));
        assert!(output.contains("Node=n1 StableId=42 Endpoint=1 InventoryGeneration=3"));
    }

    #[test]
    fn rejects_untrusted_manifest_fields_and_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("topology.json");
        for document in [
            r#"{"version":2,"namespace":"test","blocks":[]}"#,
            r#"{"version":1,"namespace":"test","blocks":[],"ready":true}"#,
        ] {
            std::fs::write(&path, document).unwrap();
            assert!(read_manifest(&path).is_err());
        }
    }
}
