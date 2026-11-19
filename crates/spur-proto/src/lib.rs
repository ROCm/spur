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

pub mod codec;

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

/// gRPC metadata key an agent sets on a `ResourceExhausted` launch refusal to
/// say why; an older controller ignores it and keeps its generic path.
pub const LAUNCH_REFUSAL_HEADER: &str = "spur-launch-refusal";

/// [`LAUNCH_REFUSAL_HEADER`] value: the GPU's reserved CPUs are held by a job
/// from before a core-layout change, which the refused job did not cause.
pub const REFUSAL_RESERVED_CPUS_HELD: &str = "reserved-cpus-held";

/// [`LAUNCH_REFUSAL_HEADER`] value: a typed step's share on a node is held by
/// the job's other steps, so the step stays runnable and srun retries it.
pub const REFUSAL_STEP_SHARE_BUSY: &str = "step-share-busy";

fn has_launch_refusal(status: &tonic::Status, value: &str) -> bool {
    status
        .metadata()
        .get(LAUNCH_REFUSAL_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(value))
}

/// Whether `status` refuses a launch over reserved CPUs held under an older layout.
pub fn is_reserved_cpus_held_refusal(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::ResourceExhausted
        && has_launch_refusal(status, REFUSAL_RESERVED_CPUS_HELD)
}

/// Builds a `ResourceExhausted` status marked as a busy typed-step share, so
/// callers can tell it apart from any other resource refusal.
pub fn step_share_busy(message: impl Into<String>) -> tonic::Status {
    let mut status = tonic::Status::resource_exhausted(message);
    status.metadata_mut().insert(
        LAUNCH_REFUSAL_HEADER,
        tonic::metadata::MetadataValue::from_static(REFUSAL_STEP_SHARE_BUSY),
    );
    status
}

/// Whether `status` refuses a launch because a typed step's share is busy.
pub fn is_step_share_busy(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::ResourceExhausted
        && has_launch_refusal(status, REFUSAL_STEP_SHARE_BUSY)
}

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
    fn step_resource_additions_preserve_wire_compatibility() {
        #[derive(Clone, PartialEq, Message)]
        struct LegacyStepResources {
            #[prost(uint32, optional, tag = "1")]
            cpus_per_task: Option<u32>,
            #[prost(uint64, optional, tag = "2")]
            memory_mb: Option<u64>,
        }

        let old_wire = LegacyStepResources {
            cpus_per_task: Some(2),
            memory_mb: Some(1024),
        }
        .encode_to_vec();
        let decoded = super::StepResources::decode(old_wire.as_slice()).unwrap();
        assert_eq!(decoded.cpus_per_task, Some(2));
        assert_eq!(decoded.cpus_per_gpu, None);
        assert_eq!(decoded.memory_per_gpu_mb, None);
        assert_eq!(decoded.memory_per_cpu_mb, None);
        assert!(decoded.gpu_type.is_empty());

        let new = super::StepResources {
            cpus_per_gpu: Some(8),
            memory_per_gpu_mb: Some(4096),
            memory_per_cpu_mb: Some(512),
            gpu_type: "mi355x".into(),
            ..decoded
        };
        let new_wire = new.encode_to_vec();
        assert_eq!(
            super::StepResources::decode(new_wire.as_slice()).unwrap(),
            new
        );
        assert_eq!(
            LegacyStepResources::decode(new_wire.as_slice()).unwrap(),
            LegacyStepResources::decode(old_wire.as_slice()).unwrap(),
            "an old peer still reads the fields it knows"
        );

        let response = super::RunCommandResponse::decode(b"\x08\x89\x01".as_slice()).unwrap();
        assert_eq!(response.exit_code, 137);
        assert!(!response.oom_killed);
    }

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
            trimmed_cpus: vec![34, 35],
        };
        let revision = crate::proto::LayoutRevision {
            term: 3,
            boot: 2,
            seq: 42,
        };
        let resp = crate::proto::RegisterAgentResponse {
            accepted: true,
            core_layout: Some(layout.clone()),
            layout_revision: Some(revision),
            ..Default::default()
        };
        let decoded =
            crate::proto::RegisterAgentResponse::decode(resp.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.core_layout, Some(layout));
        assert_eq!(decoded.layout_revision, Some(revision));
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
    fn blocked_cpus_round_trip_and_default_to_none_blocked() {
        use prost::Message;
        let req = crate::proto::HeartbeatRequest {
            hostname: "n1".into(),
            layout_blocked_cpus: vec![crate::proto::GpuBlockedCpus {
                stable_id: 7,
                cpus: 4,
            }],
            ..Default::default()
        };
        let back = crate::proto::HeartbeatRequest::decode(req.encode_to_vec().as_slice()).unwrap();
        assert_eq!(back.layout_blocked_cpus, req.layout_blocked_cpus);
        let old = crate::proto::HeartbeatRequest {
            hostname: "n1".into(),
            ..Default::default()
        };
        let decoded =
            crate::proto::HeartbeatRequest::decode(old.encode_to_vec().as_slice()).unwrap();
        assert!(decoded.layout_blocked_cpus.is_empty());
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

        let step_overlap = crate::proto::RunStepRequest {
            overlap: true,
            ..Default::default()
        };
        assert_eq!(step_overlap.encode_to_vec(), [0x70, 0x01]);
        let command_overlap = crate::proto::RunCommandRequest {
            overlap: true,
            ..Default::default()
        };
        assert_eq!(command_overlap.encode_to_vec(), [0x98, 0x01, 0x01]);
        let oom = crate::proto::RunCommandResponse {
            oom_killed: true,
            ..Default::default()
        };
        assert_eq!(oom.encode_to_vec(), [0x20, 0x01]);
        let gpu_type = crate::proto::StepResources {
            gpu_type: "a".into(),
            ..Default::default()
        };
        assert_eq!(gpu_type.encode_to_vec(), [0x4A, 0x01, b'a']);
        let capabilities = crate::proto::PingResponse {
            capabilities: vec!["a".into()],
            ..Default::default()
        };
        assert_eq!(capabilities.encode_to_vec(), [0x6A, 0x01, b'a']);
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

    #[test]
    fn a_refusal_trailer_is_recognised_only_with_its_value() {
        use tonic::metadata::MetadataValue;
        let mut status = tonic::Status::resource_exhausted("held");
        assert!(!crate::is_reserved_cpus_held_refusal(&status));
        status.metadata_mut().insert(
            crate::LAUNCH_REFUSAL_HEADER,
            MetadataValue::from_static(crate::REFUSAL_RESERVED_CPUS_HELD),
        );
        assert!(crate::is_reserved_cpus_held_refusal(&status));

        let mut other = tonic::Status::resource_exhausted("held");
        other.metadata_mut().insert(
            crate::LAUNCH_REFUSAL_HEADER,
            MetadataValue::from_static("other"),
        );
        assert!(!crate::is_reserved_cpus_held_refusal(&other));

        let mut wrong_code = tonic::Status::internal("held");
        wrong_code.metadata_mut().insert(
            crate::LAUNCH_REFUSAL_HEADER,
            MetadataValue::from_static(crate::REFUSAL_RESERVED_CPUS_HELD),
        );
        assert!(!crate::is_reserved_cpus_held_refusal(&wrong_code));
    }

    #[test]
    fn a_share_busy_refusal_is_recognised_only_with_its_marker() {
        let busy = crate::step_share_busy("held by step 3");
        assert!(crate::is_step_share_busy(&busy));
        assert!(!crate::is_reserved_cpus_held_refusal(&busy));
        assert!(!crate::is_step_share_busy(
            &tonic::Status::resource_exhausted("cleanup pending")
        ));
        let mut wrong_code = tonic::Status::failed_precondition("x");
        wrong_code.metadata_mut().insert(
            crate::LAUNCH_REFUSAL_HEADER,
            tonic::metadata::MetadataValue::from_static(crate::REFUSAL_STEP_SHARE_BUSY),
        );
        assert!(!crate::is_step_share_busy(&wrong_code));
    }

    #[test]
    fn the_refusal_trailer_match_ignores_whitespace_and_ascii_case() {
        use tonic::metadata::MetadataValue;

        let mut padded_and_cased = tonic::Status::resource_exhausted("held");
        padded_and_cased.metadata_mut().insert(
            crate::LAUNCH_REFUSAL_HEADER,
            MetadataValue::from_static(" Reserved-CPUs-Held "),
        );
        assert!(crate::is_reserved_cpus_held_refusal(&padded_and_cased));

        let mut unrelated = tonic::Status::resource_exhausted("held");
        unrelated.metadata_mut().insert(
            crate::LAUNCH_REFUSAL_HEADER,
            MetadataValue::from_static("something-else"),
        );
        assert!(!crate::is_reserved_cpus_held_refusal(&unrelated));

        let missing_header = tonic::Status::resource_exhausted("held");
        assert!(!crate::is_reserved_cpus_held_refusal(&missing_header));
    }
}
