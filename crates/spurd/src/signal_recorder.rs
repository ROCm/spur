// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A real workload that counts the SIGTERMs (and SIGUSR1s, a signal that does
//! not make a cancel wait for exit) it receives, for tests that must tell one
//! delivery from two.

/// Counts what a workload saw: one line in `term` per SIGTERM, one in `usr1`
/// per SIGUSR1 and one in `beat` per loop pass. A pending trap runs before the
/// shell's next command, so two beats after a signal was sent prove it has been
/// handled.
pub(crate) struct SignalRecorder(tempfile::TempDir);

impl SignalRecorder {
    /// The workload leads its own process group, as real jobs and steps do, so
    /// group-targeted signals reach it.
    pub(crate) async fn spawn() -> (Self, tokio::process::Child) {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = format!(
            "trap 'echo >> {dir}/term' TERM; trap 'echo >> {dir}/usr1' USR1; \
             while :; do echo >> {dir}/beat; sleep 0.02; done",
            dir = dir.path().display()
        );
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn recorder");
        let recorder = Self(dir);
        recorder.wait_for("beat", 1).await;
        (recorder, child)
    }

    fn lines(&self, name: &str) -> usize {
        std::fs::read_to_string(self.0.path().join(name))
            .map(|contents| contents.lines().count())
            .unwrap_or(0)
    }

    pub(crate) async fn wait_for(&self, name: &str, lines: usize) {
        while self.lines(name) < lines {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    pub(crate) async fn settle(&self) {
        self.wait_for("beat", self.lines("beat") + 2).await;
    }

    pub(crate) fn sigterms(&self) -> usize {
        self.lines("term")
    }
}
