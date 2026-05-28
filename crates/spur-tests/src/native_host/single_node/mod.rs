// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod container;
pub mod jobs;
pub mod lifecycle;

use tokio::sync::OnceCell;

use crate::native_host::config::TestConfig;
use crate::native_host::fixture::NativeHostFixture;

static FIXTURE: OnceCell<NativeHostFixture> = OnceCell::const_new();

pub async fn fixture() -> &'static NativeHostFixture {
    FIXTURE
        .get_or_init(|| async {
            let config = TestConfig::from_env().expect("SPUR_TEST_BM_* config");
            NativeHostFixture::deploy(config)
                .await
                .expect("failed to deploy native-host cluster for single_node tests")
        })
        .await
}
