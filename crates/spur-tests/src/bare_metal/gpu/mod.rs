// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod multi_node;
pub mod single_node;

use tokio::sync::OnceCell;

use crate::bare_metal::config::TestConfig;
use crate::bare_metal::fixture::BareMetalFixture;

static FIXTURE: OnceCell<BareMetalFixture> = OnceCell::const_new();

pub async fn fixture() -> &'static BareMetalFixture {
    FIXTURE
        .get_or_init(|| async {
            let config = TestConfig::from_env().expect("SPUR_TEST_BM_* config");
            if config.nodes.len() < 2 {
                panic!(
                    "bare_metal::gpu requires at least 2 nodes in SPUR_TEST_BM_NODES (got {})",
                    config.nodes.len()
                );
            }
            BareMetalFixture::deploy(config)
                .await
                .expect("failed to deploy bare-metal cluster for gpu tests")
        })
        .await
}
