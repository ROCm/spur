// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod address;
pub mod comm_addr;
pub mod detect;
pub mod mesh;
pub mod oci;
pub mod wireguard;

pub use address::{AddressPool, AddressSource, NodeAddress};
pub use comm_addr::{
    comm_addr_is_unusable, comm_host_for_socket, format_comm_http_url, format_comm_socket,
    is_loopback_ip, is_unusable_comm_ip, normalize_comm_address, validate_comm_address,
    CommAddressError,
};
pub use detect::detect_node_address;
pub use mesh::{MeshMembership, MeshNode};
pub use oci::{pull_image, ImageRef};
pub use wireguard::{WgConfig, WgPeer};
