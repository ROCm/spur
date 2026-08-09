// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! PMIx MPI plugin (`spur_mpi_pmix.so`) loaded at `--mpi=pmix` job launch.
//!
//! The shared library is built from C in `build.rs`.

#[cfg(test)]
mod tests {
    #[test]
    fn modex_server_stop_releases_listener() {
        let exe = std::path::PathBuf::from(env!("OUT_DIR")).join("modex_exchange_test");
        assert!(
            exe.exists(),
            "modex_exchange_test binary missing at {}",
            exe.display()
        );
        let status = std::process::Command::new(exe)
            .status()
            .expect("run modex_exchange_test");
        assert!(status.success(), "modex_exchange_test failed: {status:?}");
    }
}
