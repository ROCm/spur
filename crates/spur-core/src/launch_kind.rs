// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! How a job launches, and the decisions that follow from it.

#![deny(clippy::wildcard_enum_match_arm)]

use crate::job::JobSpec;

/// How a job reaches its nodes and what its run consists of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
    /// A script the nodes run: sbatch, FFI and REST submissions.
    Batch,
    /// Nodes held for a client that runs its work as steps.
    Allocation(AllocationOrigin),
    /// Scriptless work outside an allocation: argv is all it runs. This describes the spec, not
    /// its source; a SpurJob is one such job, but so is any argv-only submission.
    Workload,
}

/// The command holding an allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationOrigin {
    Salloc,
    /// Standalone srun; with `pty`, the allocation ends with its interactive session.
    Srun {
        pty: bool,
    },
}

/// What a failed prolog does to the job it was preparing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrologFailure {
    /// End the job: its client is waiting on a launch that will not come.
    Cancel,
    /// Keep the job for another attempt; the caller decides whether it is held or requeued.
    Retry,
}

impl LaunchKind {
    pub const ALL: [Self; 5] = [
        Self::Batch,
        Self::Allocation(AllocationOrigin::Salloc),
        Self::Allocation(AllocationOrigin::Srun { pty: false }),
        Self::Allocation(AllocationOrigin::Srun { pty: true }),
        Self::Workload,
    ];

    /// Kept off Kubernetes virtual nodes, whose agents launch only SpurJobs.
    pub fn needs_native_nodes(self) -> bool {
        match self {
            Self::Batch => true,
            Self::Allocation(_) | Self::Workload => false,
        }
    }

    /// A client keeps the job alive with `JobKeepalive`, and `InactiveLimit` reaps the job
    /// once that client goes quiet.
    pub fn client_attached(self) -> bool {
        match self {
            Self::Allocation(_) => true,
            Self::Batch | Self::Workload => false,
        }
    }

    /// Whether `RequeueJob` may send the job's run back to the queue: a client attached to
    /// an allocation cannot follow it into a new run.
    pub fn requeue_allowed(self) -> bool {
        match self {
            Self::Batch | Self::Workload => true,
            Self::Allocation(_) => false,
        }
    }

    pub fn on_prolog_failure(self) -> PrologFailure {
        match self {
            Self::Allocation(_) => PrologFailure::Cancel,
            Self::Batch | Self::Workload => PrologFailure::Retry,
        }
    }

    /// Whether submit must reject the job when its argv is empty: with no script, argv is
    /// all there is to run.
    pub fn needs_command(self) -> bool {
        match self {
            Self::Workload => true,
            Self::Batch | Self::Allocation(_) => false,
        }
    }

    /// Whether dispatch registers the allocation instead of launching the job, given whether all
    /// its nodes are native agents. A dispatched run keeps what its `JobStart` recorded.
    pub fn registers_allocation(self, all_native: bool) -> bool {
        match self {
            Self::Allocation(AllocationOrigin::Srun { .. }) => all_native,
            Self::Batch | Self::Allocation(AllocationOrigin::Salloc) | Self::Workload => false,
        }
    }
}

/// The spec fields a [`LaunchKind`] is read from, taken from either form of the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchFlags {
    interactive: bool,
    srun_job: bool,
    pty: bool,
    has_script: bool,
}

impl LaunchFlags {
    /// salloc wins over srun, and `pty` only refines an srun allocation, so every flag
    /// combination has exactly one kind.
    pub fn kind(self) -> LaunchKind {
        if self.interactive {
            LaunchKind::Allocation(AllocationOrigin::Salloc)
        } else if self.srun_job {
            LaunchKind::Allocation(AllocationOrigin::Srun { pty: self.pty })
        } else if self.has_script {
            LaunchKind::Batch
        } else {
            LaunchKind::Workload
        }
    }

    /// The launch only holds its node for an interactive session that starts later. srun's
    /// fallback dispatch clears `srun_job`, so this reads `pty` alone.
    pub fn holds_for_interactive_session(self) -> bool {
        self.pty
    }
}

impl From<&JobSpec> for LaunchFlags {
    fn from(spec: &JobSpec) -> Self {
        Self {
            interactive: spec.interactive,
            srun_job: spec.srun_job,
            pty: spec.pty,
            has_script: spec.script.is_some(),
        }
    }
}

/// An empty proto script is no script, as the controller stores it.
impl From<&spur_proto::proto::JobSpec> for LaunchFlags {
    fn from(spec: &spur_proto::proto::JobSpec) -> Self {
        Self {
            interactive: spec.interactive,
            srun_job: spec.srun_job,
            pty: spec.pty,
            has_script: !spec.script.is_empty(),
        }
    }
}

impl JobSpec {
    pub fn launch_kind(&self) -> LaunchKind {
        LaunchFlags::from(self).kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SALLOC: LaunchKind = LaunchKind::Allocation(AllocationOrigin::Salloc);
    const SRUN: LaunchKind = LaunchKind::Allocation(AllocationOrigin::Srun { pty: false });
    const SRUN_PTY: LaunchKind = LaunchKind::Allocation(AllocationOrigin::Srun { pty: true });

    fn spec(interactive: bool, srun_job: bool, pty: bool, script: bool) -> JobSpec {
        JobSpec {
            interactive,
            srun_job,
            pty,
            script: script.then(|| "#!/bin/bash\ntrue\n".to_string()),
            ..Default::default()
        }
    }

    fn proto_spec(
        interactive: bool,
        srun_job: bool,
        pty: bool,
        script: bool,
    ) -> spur_proto::proto::JobSpec {
        spur_proto::proto::JobSpec {
            interactive,
            srun_job,
            pty,
            script: if script { "#!/bin/bash\ntrue\n" } else { "" }.to_string(),
            ..Default::default()
        }
    }

    /// Every combination of the flags a kind is read from.
    fn every_flag_combination() -> impl Iterator<Item = (bool, bool, bool, bool)> {
        (0..16u8).map(|bits| (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0))
    }

    #[test]
    fn salloc_is_a_salloc_allocation() {
        assert_eq!(spec(true, false, false, true).launch_kind(), SALLOC);
    }

    #[test]
    fn srun_is_an_srun_allocation_with_or_without_a_pty() {
        assert_eq!(spec(false, true, false, true).launch_kind(), SRUN);
        assert_eq!(spec(false, true, true, true).launch_kind(), SRUN_PTY);
    }

    #[test]
    fn a_script_from_neither_srun_nor_salloc_is_a_batch_job() {
        assert_eq!(
            spec(false, false, false, true).launch_kind(),
            LaunchKind::Batch
        );
    }

    #[test]
    fn a_spec_without_a_script_is_a_workload() {
        assert_eq!(
            spec(false, false, false, false).launch_kind(),
            LaunchKind::Workload
        );
    }

    #[test]
    fn salloc_wins_over_srun() {
        assert_eq!(spec(true, true, true, true).launch_kind(), SALLOC);
    }

    #[test]
    fn a_pty_alone_does_not_make_an_allocation() {
        assert_eq!(
            spec(false, false, true, true).launch_kind(),
            LaunchKind::Batch
        );
        assert_eq!(
            spec(false, false, true, false).launch_kind(),
            LaunchKind::Workload
        );
    }

    #[test]
    fn a_proto_spec_classifies_like_the_spec_it_is_stored_as() {
        for (interactive, srun_job, pty, script) in every_flag_combination() {
            assert_eq!(
                LaunchFlags::from(&proto_spec(interactive, srun_job, pty, script)).kind(),
                spec(interactive, srun_job, pty, script).launch_kind(),
                "interactive={interactive} srun_job={srun_job} pty={pty} script={script}"
            );
        }
    }

    #[test]
    fn the_batch_predicates_agree_with_the_kind() {
        for (interactive, srun_job, pty, script) in every_flag_combination() {
            let spec = spec(interactive, srun_job, pty, script);
            let kind = spec.launch_kind();
            assert_eq!(spec.is_batch(), kind == LaunchKind::Batch, "{kind:?}");
            assert_eq!(
                spec.needs_native_nodes(),
                kind.needs_native_nodes(),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn all_lists_every_kind_once_in_order() {
        // An exhaustive match: a new kind does not compile here until it is placed in `ALL`.
        fn position(kind: LaunchKind) -> usize {
            match kind {
                LaunchKind::Batch => 0,
                LaunchKind::Allocation(AllocationOrigin::Salloc) => 1,
                LaunchKind::Allocation(AllocationOrigin::Srun { pty: false }) => 2,
                LaunchKind::Allocation(AllocationOrigin::Srun { pty: true }) => 3,
                LaunchKind::Workload => 4,
            }
        }
        let positions: Vec<usize> = LaunchKind::ALL.into_iter().map(position).collect();
        assert_eq!(positions, (0..LaunchKind::ALL.len()).collect::<Vec<_>>());
    }

    #[test]
    fn only_a_batch_job_needs_native_nodes() {
        let needing: Vec<_> = LaunchKind::ALL
            .into_iter()
            .filter(|kind| kind.needs_native_nodes())
            .collect();
        assert_eq!(needing, [LaunchKind::Batch]);
    }

    #[test]
    fn every_allocation_and_nothing_else_has_an_attached_client() {
        for kind in LaunchKind::ALL {
            assert_eq!(
                kind.client_attached(),
                matches!(kind, LaunchKind::Allocation(_)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn an_allocation_cannot_be_requeued() {
        for kind in LaunchKind::ALL {
            assert_eq!(
                kind.requeue_allowed(),
                !matches!(kind, LaunchKind::Allocation(_)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_failed_prolog_cancels_an_allocation_and_retries_anything_else() {
        for kind in LaunchKind::ALL {
            let expected = if matches!(kind, LaunchKind::Allocation(_)) {
                PrologFailure::Cancel
            } else {
                PrologFailure::Retry
            };
            assert_eq!(kind.on_prolog_failure(), expected, "{kind:?}");
        }
    }

    #[test]
    fn only_a_workload_needs_a_command() {
        let needing: Vec<_> = LaunchKind::ALL
            .into_iter()
            .filter(|kind| kind.needs_command())
            .collect();
        assert_eq!(needing, [LaunchKind::Workload]);
    }

    #[test]
    fn srun_registers_an_allocation_only_when_every_node_is_native() {
        for srun in [SRUN, SRUN_PTY] {
            assert!(srun.registers_allocation(true), "{srun:?}");
            assert!(!srun.registers_allocation(false), "{srun:?}");
        }
    }

    #[test]
    fn every_other_kind_is_launched_wherever_it_runs() {
        for kind in [LaunchKind::Batch, SALLOC, LaunchKind::Workload] {
            for all_native in [true, false] {
                assert!(!kind.registers_allocation(all_native), "{kind:?}");
            }
        }
    }

    #[test]
    fn a_pty_launch_holds_its_node_for_an_interactive_session() {
        // srun's fallback dispatch clears srun_job but keeps pty.
        assert!(LaunchFlags::from(&proto_spec(false, false, true, true))
            .holds_for_interactive_session());
        assert!(LaunchFlags::from(&spec(false, true, true, true)).holds_for_interactive_session());
        assert!(!LaunchFlags::from(&proto_spec(false, false, false, true))
            .holds_for_interactive_session());
    }
}
