// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use spur_core::block_file::BlockFile;
use spur_core::fabric::{FabricManifest, FabricView};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TopologyFormat {
    Json,
    Block,
}

#[derive(Debug, Subcommand)]
pub enum TopologyCommand {
    /// Show explicit fabric blocks and membership diagnostics
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Print JSON membership or BlockName/Nodes records to stdout
    Export {
        #[arg(long, value_enum, default_value = "json")]
        format: TopologyFormat,
    },
    /// Validate a candidate document against current controller inventory
    Validate {
        file: PathBuf,
        #[arg(long, value_enum, default_value = "json")]
        format: TopologyFormat,
    },
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
        TopologyCommand::Validate {
            file,
            format: TopologyFormat::Json,
        } => Some(read_manifest(file)?),
        _ => None,
    };
    let block_file = match &command {
        TopologyCommand::Validate {
            file,
            format: TopologyFormat::Block,
        } => Some(
            BlockFile::read_with_text(file)
                .map_err(anyhow::Error::msg)?
                .1,
        ),
        _ => None,
    };
    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);
    let response = if let Some(content) = block_file {
        let mut request =
            tonic::Request::new(spur_proto::proto::ValidateBlockFileRequest { content });
        request.set_timeout(std::time::Duration::from_secs(30));
        client
            .validate_block_file(request)
            .await
            .context("failed to validate block file (requires controller support)")?
    } else {
        let mut request = tonic::Request::new(());
        request.set_timeout(std::time::Duration::from_secs(30));
        client
            .get_fabric_topology(request)
            .await
            .context("failed to get fabric topology (requires controller support)")?
    };
    let view = FabricView::try_from(response.into_inner()).map_err(anyhow::Error::msg)?;
    match command {
        TopologyCommand::Show { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
            } else {
                print!("{}", format_view(&view));
            }
        }
        TopologyCommand::Export {
            format: TopologyFormat::Json,
        } => {
            if !view.unresolved.is_empty() {
                bail!("some GPUs have unresolved membership; inspect topology show --json");
            }
            let manifest = FabricManifest::from_view(&view);
            manifest.resolve(&view).map_err(anyhow::Error::msg)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        TopologyCommand::Export {
            format: TopologyFormat::Block,
        } => {
            print!("{}", export_blocks(&view)?);
        }
        TopologyCommand::Validate {
            format: TopologyFormat::Block,
            ..
        } => {
            if let Some(block) = view.blocks.iter().find(|b| !b.issues.is_empty()) {
                bail!(
                    "Invalid block {}: {}",
                    block.key.fabric_domain,
                    block.issues.join(",")
                );
            }
            println!("Valid: {} declared blocks resolve against live inventory. Validation does not install this file or verify fabric connectivity.", view.blocks.len());
        }
        TopologyCommand::Validate {
            format: TopologyFormat::Json,
            ..
        } => {
            let manifest = manifest.context("missing topology manifest")?;
            let blocks = manifest.resolve(&view).map_err(anyhow::Error::msg)?;
            println!("Valid: {} blocks match current registered membership. This does not verify fabric health or reserve devices.", blocks.len());
        }
    }
    Ok(())
}

fn export_blocks(view: &FabricView) -> Result<String> {
    let mut text = String::new();
    for (index, block) in view.blocks.iter().enumerate() {
        if !block.issues.is_empty() {
            bail!("cannot export invalid block {}", block.key.fabric_domain);
        }
        let nodes: std::collections::BTreeSet<_> = block
            .members
            .iter()
            .map(|m| m.device.node.clone())
            .collect();
        if view
            .unresolved
            .iter()
            .any(|gpu| nodes.contains(&gpu.device.node))
        {
            bail!("cannot export a whole-node block with unresolved GPUs");
        }
        let name = if block.file_revision.is_some() {
            block.key.fabric_domain.clone()
        } else {
            format!("block{index}")
        };
        text.push_str(&format!(
            "BlockName={} Nodes={}\n",
            name,
            spur_core::hostlist::compress(&nodes.into_iter().collect::<Vec<_>>())
        ));
    }
    BlockFile::parse(&text).map_err(anyhow::Error::msg)?;
    Ok(text)
}

fn format_view(view: &FabricView) -> String {
    use std::fmt::Write;
    let mut out = format!(
        "Namespace={} Blocks={} UnresolvedGPUs={}\n",
        view.namespace,
        view.blocks.len(),
        view.unresolved.len()
    );
    let _ = writeln!(
        out,
        "SingleBlockPartitions=[{}]",
        view.single_block_partitions.join(",")
    );
    for block in &view.blocks {
        if let Some(revision) = &block.file_revision {
            let _ = writeln!(
                out,
                "BlockName={} Source=file Revision={} NativeObservation={}",
                block.key.fabric_domain,
                revision,
                block.observed_key.is_some()
            );
        }
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
    fn block_format_is_explicit_and_json_remains_the_default() {
        use crate::scontrol::{ScontrolArgs, ScontrolCommand};
        for (args, expected) in [
            (vec!["control", "topology", "export"], TopologyFormat::Json),
            (
                vec!["control", "topology", "export", "--format", "block"],
                TopologyFormat::Block,
            ),
            (
                vec![
                    "control",
                    "topology",
                    "validate",
                    "candidate.conf",
                    "--format",
                    "block",
                ],
                TopologyFormat::Block,
            ),
        ] {
            match ScontrolArgs::try_parse_from(args).unwrap().command {
                ScontrolCommand::Topology {
                    command:
                        TopologyCommand::Export { format } | TopologyCommand::Validate { format, .. },
                } => assert_eq!(format, expected),
                _ => panic!("wrong subcommand"),
            }
        }
    }

    #[test]
    fn formats_scoped_members_and_failures() {
        let view = FabricView {
            single_block_partitions: vec![],
            namespace: "test".into(),
            unresolved: Vec::new(),
            blocks: vec![FabricBlock {
                file_revision: None,
                observed_key: None,
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
        assert!(export_blocks(&view).is_err());
        let mut view = view;
        view.blocks[0].issues.clear();
        assert_eq!(export_blocks(&view).unwrap(), "BlockName=block0 Nodes=n1\n");
        view.blocks.push(view.blocks[0].clone());
        assert!(export_blocks(&view).is_err());
        view.blocks.pop();
        view.unresolved
            .push(spur_core::fabric::UnresolvedFabricDevice {
                device: FabricDeviceKey {
                    node: "n1".into(),
                    stable_id: 43,
                },
                issue: "missing".into(),
            });
        assert!(export_blocks(&view).is_err());
    }

    #[tokio::test]
    async fn block_validation_preserves_bounded_text_across_the_rpc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("topology.conf");
        let text = format!(
            "# compact range before suffix\nNodes={}[00000-04999]x BlockName=a\n",
            "n".repeat(240)
        );
        std::fs::write(&path, &text).unwrap();
        let parsed = BlockFile::read(&path).unwrap();
        assert_eq!(parsed.blocks[0].nodes.len(), 5000);
        assert!(parsed.to_text().len() > spur_core::block_file::MAX_BLOCK_FILE_BYTES);
        let (addr, capture) = crate::mock_controller::spawn().await;
        let controller = format!("http://{addr}");
        run(
            &controller,
            TopologyCommand::Validate {
                file: path.clone(),
                format: TopologyFormat::Block,
            },
        )
        .await
        .unwrap();
        assert_eq!(capture.block_validation_content(), [text]);
        for invalid in [
            "BlockName=a Nodes=n1,n1".to_string(),
            "#".repeat(spur_core::block_file::MAX_BLOCK_FILE_BYTES + 1),
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(run(
                &controller,
                TopologyCommand::Validate {
                    file: path.clone(),
                    format: TopologyFormat::Block
                }
            )
            .await
            .is_err());
            assert_eq!(capture.block_validation_content().len(), 1);
        }
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
