// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod proto {
    // Generated code: the InteractiveInput oneof carries InitSession inline, which
    // is legitimately larger than its byte/int variants.
    #![allow(clippy::large_enum_variant)]
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

/// Transient and transport failures, including a client-side request timeout,
/// are retried; a spec rejection would repeat forever, so it is not.
pub fn controller_rpc_retryable(status: &tonic::Status) -> bool {
    use tonic::Code;
    // tonic maps a request timeout to Cancelled, hyper transport errors other
    // than a failed connect to Unknown, and an h2 GOAWAY from a dying peer to
    // Internal.
    matches!(
        status.code(),
        Code::Unavailable
            | Code::Internal
            | Code::DeadlineExceeded
            | Code::Unknown
            | Code::Cancelled
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
    use prost::Message;

    #[test]
    fn run_step_warnings_preserve_wire_compatibility() {
        #[derive(Clone, PartialEq, Message)]
        struct LegacyRunStepResponse {
            #[prost(int32, tag = "1")]
            exit_code: i32,
            #[prost(string, tag = "2")]
            stdout: String,
            #[prost(string, tag = "3")]
            stderr: String,
            #[prost(string, tag = "4")]
            node: String,
        }

        let old_wire = b"\x08\x07\x12\x04out\n\x1a\x04err\n\x22\x02n1";
        let mut response = super::RunStepResponse::decode(old_wire.as_slice()).unwrap();
        assert_eq!(response.exit_code, 7);
        assert_eq!(response.stdout, "out\n");
        assert_eq!(response.stderr, "err\n");
        assert_eq!(response.node, "n1");
        assert!(response.warnings.is_empty());

        response.warnings = vec!["output from n1 is no longer available".into()];
        let new_wire = response.encode_to_vec();
        let old_response = LegacyRunStepResponse::decode(new_wire.as_slice()).unwrap();
        assert_eq!(
            old_response,
            LegacyRunStepResponse::decode(old_wire.as_slice()).unwrap()
        );
    }

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
        assert!(!controller_rpc_retryable(&tonic::Status::ok("")));
        assert!(!controller_rpc_retryable(
            &tonic::Status::failed_precondition("not the leader")
        ));
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
        // What tonic returns when the client-side request bound expires.
        assert!(controller_rpc_retryable(&tonic::Status::cancelled(
            "Timeout expired"
        )));
    }

    #[test]
    fn a_register_request_from_an_old_agent_has_no_topology() {
        use prost::Message;
        let old = crate::proto::RegisterAgentRequest {
            hostname: "n1".into(),
            ..Default::default()
        };
        let decoded =
            crate::proto::RegisterAgentRequest::decode(old.encode_to_vec().as_slice()).unwrap();
        assert!(decoded.topology.is_none());
    }

    #[test]
    fn a_core_layout_survives_the_wire() {
        use prost::Message;
        let layout = crate::proto::CoreLayout {
            spec_cpus: vec![127, 255],
            mem_spec_mb: 16384,
            gpu_reserved: vec![crate::proto::GpuReservedCpus {
                stable_id: 7,
                cpus: vec![0, 128],
            }],
            general_cpus: vec![32, 33],
            daemons_off_spec: true,
        };
        let resp = crate::proto::RegisterAgentResponse {
            accepted: true,
            core_layout: Some(layout.clone()),
            ..Default::default()
        };
        let decoded =
            crate::proto::RegisterAgentResponse::decode(resp.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.core_layout, Some(layout));
    }

    #[test]
    fn an_empty_heartbeat_reply_does_not_ask_to_reregister() {
        use prost::Message;
        let decoded = crate::proto::HeartbeatResponse::decode(&[][..]).unwrap();
        assert!(!decoded.reregister);
    }

    #[test]
    fn a_cpu_topology_keeps_numa_presence() {
        use prost::Message;
        let topology = crate::proto::CpuTopology {
            cores: vec![
                crate::proto::CoreInfo {
                    socket: 0,
                    numa_node: Some(0),
                    cpus: vec![0, 128],
                },
                crate::proto::CoreInfo {
                    socket: 1,
                    numa_node: None,
                    cpus: vec![64, 192],
                },
            ],
        };
        let req = crate::proto::RegisterAgentRequest {
            topology: Some(topology.clone()),
            ..Default::default()
        };
        let decoded =
            crate::proto::RegisterAgentRequest::decode(req.encode_to_vec().as_slice()).unwrap();
        let cores = decoded.topology.unwrap().cores;
        assert_eq!(cores[0].numa_node, Some(0));
        assert_eq!(cores[1].numa_node, None);
        assert_eq!(cores, topology.cores);
    }

    #[test]
    fn a_launch_request_carries_the_layout_or_none() {
        use prost::Message;
        let old = crate::proto::LaunchJobRequest {
            job_id: 7,
            ..Default::default()
        };
        let decoded =
            crate::proto::LaunchJobRequest::decode(old.encode_to_vec().as_slice()).unwrap();
        assert!(decoded.core_layout.is_none());

        let layout = crate::proto::CoreLayout {
            spec_cpus: vec![127, 255],
            general_cpus: vec![0, 1],
            ..Default::default()
        };
        let new = crate::proto::LaunchJobRequest {
            job_id: 7,
            core_layout: Some(layout.clone()),
            ..Default::default()
        };
        let decoded =
            crate::proto::LaunchJobRequest::decode(new.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.core_layout, Some(layout));
    }

    #[test]
    fn a_heartbeat_fingerprint_defaults_to_zero() {
        use prost::Message;
        let old = crate::proto::HeartbeatRequest {
            hostname: "n1".into(),
            ..Default::default()
        };
        let decoded =
            crate::proto::HeartbeatRequest::decode(old.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.core_layout_fingerprint, 0);

        let new = crate::proto::HeartbeatRequest {
            core_layout_fingerprint: u64::MAX,
            ..Default::default()
        };
        let decoded =
            crate::proto::HeartbeatRequest::decode(new.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.core_layout_fingerprint, u64::MAX);
    }

    #[test]
    fn node_info_spec_fields_default_when_absent() {
        use prost::Message;
        let old = crate::proto::NodeInfo {
            name: "n1".into(),
            ..Default::default()
        };
        let decoded = crate::proto::NodeInfo::decode(old.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.spec_cpus, "");
        assert_eq!(decoded.core_spec_count, 0);
        assert_eq!(decoded.mem_spec_mb, 0);

        let new = crate::proto::NodeInfo {
            spec_cpus: "127,255".into(),
            core_spec_count: 1,
            mem_spec_mb: 16384,
            ..Default::default()
        };
        let decoded = crate::proto::NodeInfo::decode(new.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.spec_cpus, "127,255");
        assert_eq!(decoded.core_spec_count, 1);
        assert_eq!(decoded.mem_spec_mb, 16384);
    }

    // Exact bytes pin each new field's tag: a renumbered field still round-trips
    // through prost but would break old and new peers on the wire.
    #[test]
    fn new_fields_use_their_assigned_tags() {
        use prost::Message;
        let reregister = crate::proto::HeartbeatResponse { reregister: true };
        assert_eq!(reregister.encode_to_vec(), [0x08, 0x01]);

        let topology = crate::proto::RegisterAgentRequest {
            topology: Some(Default::default()),
            ..Default::default()
        };
        assert_eq!(topology.encode_to_vec(), [0x4A, 0x00]);

        let reply_layout = crate::proto::RegisterAgentResponse {
            core_layout: Some(Default::default()),
            ..Default::default()
        };
        assert_eq!(reply_layout.encode_to_vec(), [0x22, 0x00]);

        let launch_layout = crate::proto::LaunchJobRequest {
            core_layout: Some(Default::default()),
            ..Default::default()
        };
        assert_eq!(launch_layout.encode_to_vec(), [0x72, 0x00]);

        let fingerprint = crate::proto::HeartbeatRequest {
            core_layout_fingerprint: 1,
            ..Default::default()
        };
        assert_eq!(fingerprint.encode_to_vec(), [0x40, 0x01]);

        let spec_cpus = crate::proto::NodeInfo {
            spec_cpus: "1".into(),
            ..Default::default()
        };
        assert_eq!(spec_cpus.encode_to_vec(), [0xDA, 0x03, 0x01, b'1']);
        let core_spec_count = crate::proto::NodeInfo {
            core_spec_count: 1,
            ..Default::default()
        };
        assert_eq!(core_spec_count.encode_to_vec(), [0xE0, 0x03, 0x01]);
        let mem_spec_mb = crate::proto::NodeInfo {
            mem_spec_mb: 1,
            ..Default::default()
        };
        assert_eq!(mem_spec_mb.encode_to_vec(), [0xE8, 0x03, 0x01]);
    }

    #[test]
    fn numa_node_presence_is_on_the_wire() {
        use prost::Message;
        let known = crate::proto::CoreInfo {
            numa_node: Some(0),
            ..Default::default()
        };
        assert_eq!(known.encode_to_vec(), [0x10, 0x00]);
        let unknown = crate::proto::CoreInfo::default();
        assert!(unknown.encode_to_vec().is_empty());
    }

    #[test]
    fn a_truncated_layout_fails_to_decode() {
        use prost::Message;
        let layout = crate::proto::CoreLayout {
            spec_cpus: vec![127, 255],
            general_cpus: vec![32, 33],
            ..Default::default()
        };
        let mut bytes = layout.encode_to_vec();
        bytes.pop();
        assert!(crate::proto::CoreLayout::decode(bytes.as_slice()).is_err());
    }
}
