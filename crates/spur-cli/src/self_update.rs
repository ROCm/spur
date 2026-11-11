// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use spur_update::check::Channel;

#[derive(Debug, Parser)]
#[command(
    name = "spur self-update",
    about = "Download and replace Spur binaries on this host",
    after_help = "Alias: spur update. To change jobs or nodes, use spur control update."
)]
pub struct SelfUpdateArgs {
    /// Install the latest nightly build instead of the latest stable release
    #[arg(long)]
    nightly: bool,
}

pub async fn main_with_args(args: Vec<String>) -> anyhow::Result<()> {
    let args: SelfUpdateArgs = crate::clap_exit::parse_or_exit(&args);
    let channel = if args.nightly {
        Channel::Nightly
    } else {
        Channel::Stable
    };
    spur_update::self_update_cli(
        "ROCm/spur",
        env!("CARGO_PKG_VERSION"),
        &channel,
        spur_update::SPUR_BINARIES,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn accepts_stable_and_nightly_for_both_aliases() {
        for command in ["self-update", "update"] {
            assert!(!SelfUpdateArgs::try_parse_from([command]).unwrap().nightly);
            assert!(
                SelfUpdateArgs::try_parse_from([command, "--nightly"])
                    .unwrap()
                    .nightly
            );
        }
    }

    #[test]
    fn rejects_unrecognized_arguments() {
        for args in [
            vec!["update", "node", "n1", "state=resume"],
            vec!["update", "node", "n1", "--nightly"],
            vec!["self-update", "--unknown"],
        ] {
            assert_eq!(
                SelfUpdateArgs::try_parse_from(args).unwrap_err().kind(),
                ErrorKind::UnknownArgument
            );
        }
    }
}
