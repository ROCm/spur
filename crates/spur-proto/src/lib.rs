// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod proto {
    tonic::include_proto!("slurm");
}

pub mod raft_proto {
    tonic::include_proto!("raft_internal");
}

pub use proto::*;

use tonic::transport::Channel;

/// Maximum size of a gRPC *response* (server encode / client decode), in bytes.
/// Large `GetJobs`/`GetNodes` responses can exceed tonic's default 4 MiB on big
/// clusters. The Raft-internal service uses a separate constant in `raft.rs`.
pub const MAX_GRPC_MESSAGE_SIZE: usize = 32 * 1024 * 1024;

/// Maximum size of a gRPC *request* (client encode / server decode), in bytes.
/// Sized to the 4 MiB `JobSpec` submission cap plus headroom for proto framing;
/// no client RPC legitimately needs more. Keeps the inbound decode surface tight
/// while allowing large outbound responses.
pub const MAX_GRPC_REQUEST_SIZE: usize = 8 * 1024 * 1024;

/// Controller client with asymmetric size limits: requests capped at
/// `MAX_GRPC_REQUEST_SIZE`, responses up to `MAX_GRPC_MESSAGE_SIZE`.
pub fn controller_client(
    channel: Channel,
) -> proto::slurm_controller_client::SlurmControllerClient<Channel> {
    proto::slurm_controller_client::SlurmControllerClient::new(channel)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_GRPC_REQUEST_SIZE)
}

/// Accounting client with asymmetric size limits.
pub fn accounting_client(
    channel: Channel,
) -> proto::slurm_accounting_client::SlurmAccountingClient<Channel> {
    proto::slurm_accounting_client::SlurmAccountingClient::new(channel)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_GRPC_REQUEST_SIZE)
}

/// Controller server: inbound requests capped at `MAX_GRPC_REQUEST_SIZE`,
/// outbound responses up to `MAX_GRPC_MESSAGE_SIZE`.
pub fn controller_server<T: proto::slurm_controller_server::SlurmController>(
    service: T,
) -> proto::slurm_controller_server::SlurmControllerServer<T> {
    proto::slurm_controller_server::SlurmControllerServer::new(service)
        .max_decoding_message_size(MAX_GRPC_REQUEST_SIZE)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_SIZE)
}

/// Agent server: inbound requests capped at `MAX_GRPC_REQUEST_SIZE`,
/// outbound responses up to `MAX_GRPC_MESSAGE_SIZE`.
pub fn agent_server<T: proto::slurm_agent_server::SlurmAgent>(
    service: T,
) -> proto::slurm_agent_server::SlurmAgentServer<T> {
    proto::slurm_agent_server::SlurmAgentServer::new(service)
        .max_decoding_message_size(MAX_GRPC_REQUEST_SIZE)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_SIZE)
}

/// Accounting server: inbound requests capped at `MAX_GRPC_REQUEST_SIZE`,
/// outbound responses up to `MAX_GRPC_MESSAGE_SIZE`.
pub fn accounting_server<T: proto::slurm_accounting_server::SlurmAccounting>(
    service: T,
) -> proto::slurm_accounting_server::SlurmAccountingServer<T> {
    proto::slurm_accounting_server::SlurmAccountingServer::new(service)
        .max_decoding_message_size(MAX_GRPC_REQUEST_SIZE)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_SIZE)
}
