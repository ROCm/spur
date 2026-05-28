// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod multi_node;
pub mod single_node;

use tokio::sync::OnceCell;

use crate::native_host::config::TestConfig;
use crate::native_host::fixture::NativeHostFixture;

static FIXTURE: OnceCell<NativeHostFixture> = OnceCell::const_new();

pub async fn fixture() -> &'static NativeHostFixture {
    FIXTURE
        .get_or_init(|| async {
            let config = TestConfig::from_env().expect("SPUR_TEST_BM_* config");
            if config.nodes.len() < 2 {
                panic!(
                    "native_host::gpu requires at least 2 nodes in SPUR_TEST_BM_NODES (got {})",
                    config.nodes.len()
                );
            }
            NativeHostFixture::deploy(config)
                .await
                .expect("failed to deploy native-host cluster for gpu tests")
        })
        .await
}
