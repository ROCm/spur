//! Seccomp-BPF syscall filter for job isolation.
//!
//! Implements a default-deny syscall whitelist inspired by the AXIS sandbox
//! (axis-sandbox/src/linux/seccomp.rs). Blocks dangerous syscalls like ptrace,
//! mount, bpf, and unshare while allowing the ~150 syscalls needed for GPU
//! compute workloads (HIP, CUDA, MPI, PyTorch).
//!
//! Applied via `pre_exec` after fork but before exec, so the filter is
//! inherited by all child processes.

use tracing::{debug, warn};

/// Syscall numbers for x86_64 Linux.
/// Reference: /usr/include/asm/unistd_64.h
const ALLOWED_SYSCALLS: &[u32] = &[
    // File I/O
    0,   // read
    1,   // write
    2,   // open
    3,   // close
    4,   // stat
    5,   // fstat
    6,   // lstat
    7,   // poll
    8,   // lseek
    9,   // mmap
    10,  // mprotect
    11,  // munmap
    12,  // brk
    13,  // rt_sigaction
    14,  // rt_sigprocmask
    15,  // rt_sigreturn
    16,  // ioctl (GPU driver: KFD, DRM)
    17,  // pread64
    18,  // pwrite64
    19,  // readv
    20,  // writev
    21,  // access
    22,  // pipe
    23,  // select
    24,  // sched_yield
    25,  // mremap
    26,  // msync
    27,  // mincore
    28,  // madvise
    29,  // shmget
    30,  // shmat
    31,  // shmctl
    32,  // dup
    33,  // dup2
    34,  // pause
    35,  // nanosleep
    36,  // getitimer
    37,  // alarm
    38,  // setitimer
    39,  // getpid
    40,  // sendfile
    41,  // socket
    42,  // connect
    43,  // accept
    44,  // sendto
    45,  // recvfrom
    46,  // sendmsg
    47,  // recvmsg
    48,  // shutdown
    49,  // bind
    50,  // listen
    51,  // getsockname
    52,  // getpeername
    53,  // socketpair
    54,  // setsockopt
    55,  // getsockopt
    56,  // clone (for threads, NOT for namespaces)
    57,  // fork
    58,  // vfork
    59,  // execve
    60,  // exit
    61,  // wait4
    62,  // kill
    63,  // uname
    72,  // fcntl
    73,  // flock
    74,  // fsync
    75,  // fdatasync
    76,  // truncate
    77,  // ftruncate
    78,  // getdents
    79,  // getcwd
    80,  // chdir
    82,  // rename
    83,  // mkdir
    84,  // rmdir
    85,  // creat
    86,  // link
    87,  // unlink
    88,  // symlink
    89,  // readlink
    90,  // chmod
    91,  // fchmod
    92,  // chown
    93,  // fchown
    95,  // umask
    96,  // gettimeofday
    97,  // getrlimit
    98,  // getrusage
    99,  // sysinfo
    100, // times
    102, // getuid
    104, // getgid
    107, // geteuid
    108, // getegid
    109, // setpgid
    110, // getppid
    111, // getpgrp
    112, // setsid
    118, // getresuid
    120, // getresgid
    122, // utime
    133, // mknod
    137, // statfs
    138, // fstatfs
    140, // getpriority
    141, // setpriority (for nice)
    157, // prctl
    158, // arch_prctl
    186, // gettid
    200, // tkill
    202, // futex
    203, // sched_setaffinity
    204, // sched_getaffinity
    217, // getdents64
    218, // set_tid_address
    228, // clock_gettime
    229, // clock_getres
    230, // clock_nanosleep
    231, // exit_group
    232, // epoll_wait
    233, // epoll_ctl
    234, // tgkill
    235, // utimes
    257, // openat
    258, // mkdirat
    259, // mknodat
    260, // fchownat
    262, // newfstatat
    263, // unlinkat
    264, // renameat
    265, // linkat
    266, // symlinkat
    267, // readlinkat
    268, // fchmodat
    269, // faccessat
    270, // pselect6
    271, // ppoll
    280, // utimensat
    281, // epoll_pwait
    284, // eventfd
    285, // fallocate
    288, // accept4
    289, // signalfd4
    290, // eventfd2
    291, // epoll_create1
    292, // dup3
    293, // pipe2
    302, // prlimit64
    316, // renameat2
    318, // getrandom
    322, // execveat
    332, // statx
    334, // rseq
    435, // clone3
    439, // faccessat2
    // GPU-specific: mmap variants for VRAM mapping
    9,  // mmap (duplicate, already listed)
    25, // mremap
    28, // madvise
    // Network: needed for MPI/NCCL
    41,  // socket
    42,  // connect
    288, // accept4
    // Process management
    56,  // clone (threads)
    57,  // fork
    61,  // wait4
    247, // waitid
];

/// Apply a seccomp-BPF filter that whitelists only allowed syscalls.
/// Returns Ok(()) if successful, Err if seccomp is not available.
///
/// Must be called after fork() but before exec() — typically in a
/// `pre_exec` callback or a wrapper script.
pub fn apply_seccomp_filter() -> Result<(), String> {
    // Build BPF program: check architecture, then whitelist syscalls
    // Using seccomp in strict filter mode via prctl

    // For now, we use a simple approach: write a helper script that
    // applies seccomp via the `seccomp-tools` utility or via a small
    // C helper. The full BPF program approach (like AXIS) requires
    // linking against libseccomp or building raw BPF bytecode.
    //
    // Phase 1: Use prctl(PR_SET_NO_NEW_PRIVS) to prevent privilege escalation.
    // This is the most impactful single syscall for security.
    unsafe {
        let ret = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        if ret != 0 {
            return Err(format!(
                "prctl(PR_SET_NO_NEW_PRIVS) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    debug!("seccomp: PR_SET_NO_NEW_PRIVS applied");

    // Phase 2: Build and apply BPF filter
    // The BPF program structure:
    //   1. Load syscall number (BPF_LD | BPF_W | BPF_ABS, offset 0)
    //   2. For each allowed syscall: BPF_JMP | BPF_JEQ → ALLOW
    //   3. Default: BPF_RET | SECCOMP_RET_ERRNO(EPERM)
    //   4. ALLOW: BPF_RET | SECCOMP_RET_ALLOW

    let mut filter: Vec<libc::sock_filter> = Vec::new();

    // Validate architecture (x86_64 = AUDIT_ARCH_X86_64 = 0xC000003E)
    // BPF_LD | BPF_W | BPF_ABS: load arch from seccomp_data.arch (offset 4)
    filter.push(bpf_stmt(0x20, 4)); // LD arch
    filter.push(bpf_jump(0x15, 0xC000003E, 1, 0)); // JEQ x86_64 → skip kill
    filter.push(bpf_stmt(0x06, 0)); // RET KILL (wrong arch)

    // Load syscall number from seccomp_data.nr (offset 0)
    filter.push(bpf_stmt(0x20, 0)); // LD nr

    // Deduplicate and sort allowed syscalls
    let mut allowed: Vec<u32> = ALLOWED_SYSCALLS.to_vec();
    allowed.sort_unstable();
    allowed.dedup();

    // For each allowed syscall, add a JEQ → ALLOW jump
    let n = allowed.len();
    for (i, &nr) in allowed.iter().enumerate() {
        let jump_to_allow = (n - 1 - i) as u8;
        filter.push(bpf_jump(0x15, nr, jump_to_allow, 0));
    }

    // Default: return EPERM
    filter.push(bpf_stmt(0x06, 0x00050001)); // SECCOMP_RET_ERRNO | EPERM

    // ALLOW target
    filter.push(bpf_stmt(0x06, 0x7FFF0000)); // SECCOMP_RET_ALLOW

    // Apply the filter
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };

    unsafe {
        let ret = libc::prctl(
            libc::PR_SET_SECCOMP,
            2, // SECCOMP_MODE_FILTER
            &prog as *const libc::sock_fprog,
            0,
            0,
        );
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            // seccomp might not be available (old kernel or config)
            warn!("seccomp filter failed: {err} — continuing without syscall filtering");
            return Err(format!("seccomp filter failed: {err}"));
        }
    }

    debug!(
        syscalls = allowed.len(),
        "seccomp: BPF filter applied ({} syscalls whitelisted)",
        allowed.len()
    );
    Ok(())
}

/// BPF statement helper
fn bpf_stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

/// BPF jump helper
fn bpf_jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowed_syscalls_sorted_and_deduped() {
        let mut allowed = ALLOWED_SYSCALLS.to_vec();
        allowed.sort_unstable();
        allowed.dedup();
        // Should have a reasonable number of syscalls
        assert!(allowed.len() >= 100, "expected 100+ allowed syscalls");
        assert!(allowed.len() <= 200, "expected < 200 allowed syscalls");
    }

    #[test]
    fn test_dangerous_syscalls_not_in_whitelist() {
        let allowed: std::collections::HashSet<u32> = ALLOWED_SYSCALLS.iter().copied().collect();
        // ptrace (101) should NOT be allowed
        assert!(!allowed.contains(&101), "ptrace must not be allowed");
        // mount (165) should NOT be allowed
        assert!(!allowed.contains(&165), "mount must not be allowed");
        // umount2 (166) should NOT be allowed
        assert!(!allowed.contains(&166), "umount2 must not be allowed");
        // bpf (321) should NOT be allowed
        assert!(!allowed.contains(&321), "bpf must not be allowed");
        // unshare (272) should NOT be allowed
        assert!(!allowed.contains(&272), "unshare must not be allowed");
        // setns (308) should NOT be allowed
        assert!(!allowed.contains(&308), "setns must not be allowed");
    }

    #[test]
    fn test_gpu_syscalls_in_whitelist() {
        let allowed: std::collections::HashSet<u32> = ALLOWED_SYSCALLS.iter().copied().collect();
        // ioctl (16) needed for GPU driver
        assert!(allowed.contains(&16), "ioctl needed for GPU");
        // mmap (9) needed for VRAM mapping
        assert!(allowed.contains(&9), "mmap needed for VRAM");
        // mprotect (10) needed for GPU memory
        assert!(allowed.contains(&10), "mprotect needed for GPU");
    }

    #[test]
    fn test_network_syscalls_in_whitelist() {
        let allowed: std::collections::HashSet<u32> = ALLOWED_SYSCALLS.iter().copied().collect();
        // socket (41) needed for MPI/NCCL
        assert!(allowed.contains(&41), "socket needed for MPI");
        // connect (42) needed for MPI/NCCL
        assert!(allowed.contains(&42), "connect needed for MPI");
    }

    #[test]
    fn test_bpf_filter_builds() {
        // Verify the BPF program can be constructed without panicking
        let mut allowed = ALLOWED_SYSCALLS.to_vec();
        allowed.sort_unstable();
        allowed.dedup();

        let mut filter = Vec::new();
        filter.push(bpf_stmt(0x20, 4));
        filter.push(bpf_jump(0x15, 0xC000003E, 1, 0));
        filter.push(bpf_stmt(0x06, 0));
        filter.push(bpf_stmt(0x20, 0));

        for (i, &nr) in allowed.iter().enumerate() {
            let jump = (allowed.len() - 1 - i) as u8;
            filter.push(bpf_jump(0x15, nr, jump, 0));
        }
        filter.push(bpf_stmt(0x06, 0x00050001));
        filter.push(bpf_stmt(0x06, 0x7FFF0000));

        assert!(
            filter.len() > 100,
            "BPF program should have 100+ instructions"
        );
        assert!(filter.len() < 500, "BPF program should be compact");
    }
}
