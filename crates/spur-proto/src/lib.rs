// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod proto {
    tonic::include_proto!("slurm");
}

pub mod raft_proto {
    tonic::include_proto!("raft_internal");
}

pub use proto::*;

/// Maximum size of a gRPC *response* (server encode / client decode), in bytes.
/// Large `GetJobs`/`GetNodes` responses can exceed tonic's default 4 MiB on big
/// clusters. The Raft-internal service uses a separate constant in `raft.rs`.
pub const MAX_GRPC_MESSAGE_SIZE: usize = 32 * 1024 * 1024;

/// Maximum size of a gRPC *request* (client encode / server decode), in bytes.
/// Sized to the 4 MiB `JobSpec` submission cap plus headroom for proto framing;
/// no client RPC legitimately needs more. Keeps the inbound decode surface tight
/// while allowing large outbound responses.
pub const MAX_GRPC_REQUEST_SIZE: usize = 8 * 1024 * 1024;

/// Only transient RPC failures are retried; a spec rejection would repeat forever.
pub fn controller_rpc_retryable(status: &tonic::Status) -> bool {
    use tonic::Code;
    matches!(
        status.code(),
        Code::Unavailable | Code::Internal | Code::DeadlineExceeded | Code::Unknown
    )
}

/// Controller client with asymmetric size limits: requests capped at
/// `MAX_GRPC_REQUEST_SIZE`, responses up to `MAX_GRPC_MESSAGE_SIZE`.
pub fn controller_client<T>(channel: T) -> proto::slurm_controller_client::SlurmControllerClient<T>
where
    T: tonic::client::GrpcService<tonic::body::Body>,
    T::Error: Into<tonic::codegen::StdError>,
    T::ResponseBody:
        tonic::codegen::Body<Data = tonic::codegen::Bytes> + std::marker::Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error:
        Into<tonic::codegen::StdError> + std::marker::Send,
{
    proto::slurm_controller_client::SlurmControllerClient::new(channel)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_GRPC_REQUEST_SIZE)
}

/// Accounting client with asymmetric size limits.
pub fn accounting_client<T>(channel: T) -> proto::slurm_accounting_client::SlurmAccountingClient<T>
where
    T: tonic::client::GrpcService<tonic::body::Body>,
    T::Error: Into<tonic::codegen::StdError>,
    T::ResponseBody:
        tonic::codegen::Body<Data = tonic::codegen::Bytes> + std::marker::Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error:
        Into<tonic::codegen::StdError> + std::marker::Send,
{
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

#[cfg(test)]
mod tests {
    use super::controller_rpc_retryable;

    #[test]
    fn a_spec_rejection_is_not_retried() {
        // What spurctld returns for an unknown partition or a denied account.
        assert!(!controller_rpc_retryable(&tonic::Status::invalid_argument(
            "partition 'gpu' not found"
        )));
        assert!(!controller_rpc_retryable(
            &tonic::Status::permission_denied("account denied")
        ));
        assert!(!controller_rpc_retryable(&tonic::Status::not_found("x")));
    }

    #[test]
    fn a_controller_that_did_not_answer_is_retried() {
        // "not the Raft leader" and a failed Raft propose are both transient.
        assert!(controller_rpc_retryable(&tonic::Status::unavailable(
            "not the Raft leader"
        )));
        assert!(controller_rpc_retryable(&tonic::Status::internal(
            "raft propose failed"
        )));
        assert!(controller_rpc_retryable(&tonic::Status::deadline_exceeded(
            "timed out"
        )));
    }
}
