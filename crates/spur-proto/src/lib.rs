pub mod proto {
    tonic::include_proto!("slurm");
}

pub mod raft_proto {
    tonic::include_proto!("raft_internal");
}

pub use proto::*;
