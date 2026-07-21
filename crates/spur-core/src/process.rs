// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Exit code suitable for shell / srun step return: normal exit status, or `128 + signal` when signaled.
pub fn shell_exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|s| 128 + s).unwrap_or(-1)
    }
    #[cfg(not(unix))]
    {
        -1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn shell_exit_code_returns_process_status() {
        let status = Command::new("true").status().unwrap();
        assert_eq!(shell_exit_code(&status), 0);

        let status = Command::new("false").status().unwrap();
        assert_eq!(shell_exit_code(&status), 1);
    }
}
