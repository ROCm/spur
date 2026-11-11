// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// How one task ended, as `waitpid` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Exited(i32),
    Signaled(i32),
}

/// The result of a set of tasks, ranked the way Slurm's `srun` ranks them
/// (`_task_finish` in `src/srun/launch.c`): any signal outranks any exit code,
/// and within each kind the highest value wins.
pub fn aggregate(statuses: impl IntoIterator<Item = TaskStatus>) -> TaskStatus {
    statuses.into_iter().fold(TaskStatus::Exited(0), outranking)
}

fn outranking(a: TaskStatus, b: TaskStatus) -> TaskStatus {
    use TaskStatus::{Exited, Signaled};
    match (a, b) {
        (Signaled(x), Signaled(y)) => Signaled(x.max(y)),
        (Signaled(_), Exited(_)) => a,
        (Exited(_), Signaled(_)) => b,
        (Exited(x), Exited(y)) => Exited(x.max(y)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskStatus::{Exited, Signaled};

    #[test]
    fn no_tasks_succeed() {
        assert_eq!(aggregate([]), Exited(0));
    }

    #[test]
    fn the_highest_exit_code_wins() {
        assert_eq!(aggregate([Exited(1), Exited(3), Exited(0)]), Exited(3));
    }

    #[test]
    fn a_signal_outranks_a_higher_exit_code() {
        assert_eq!(
            aggregate([Exited(200), Signaled(9), Exited(1)]),
            Signaled(9)
        );
    }

    #[test]
    fn the_highest_signal_wins() {
        assert_eq!(
            aggregate([Signaled(9), Signaled(15), Signaled(2)]),
            Signaled(15)
        );
    }
}
