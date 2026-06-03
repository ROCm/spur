// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use kube::CustomResourceExt;

fn main() {
    let crd = spur_k8s::crd::SpurJob::crd();
    print!("{}", serde_json::to_string(&crd).unwrap());
}
