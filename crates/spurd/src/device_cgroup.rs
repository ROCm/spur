// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! cgroup-v2 device access control via a BPF_PROG_TYPE_CGROUP_DEVICE program.
//!
//! cgroup v2 dropped the `devices` controller. The program is built in-process and
//! loaded with `bpf()`, as systemd does, so no eBPF build toolchain is needed.

use std::ffi::CStr;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use anyhow::Context;

pub const ACC_MKNOD: u8 = 1;
pub const ACC_READ: u8 = 2;
pub const ACC_WRITE: u8 = 4;
const ACC_ALL: u8 = ACC_MKNOD | ACC_READ | ACC_WRITE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevType {
    Char,
    Block,
}

/// `None` major/minor matches any value, mirroring a `*` in cgroup-v1 `devices.allow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRule {
    pub dev_type: DevType,
    pub major: Option<u32>,
    pub minor: Option<u32>,
    pub access: u8,
}

impl DeviceRule {
    fn char_dev(major: u32, minor: Option<u32>) -> Self {
        Self {
            dev_type: DevType::Char,
            major: Some(major),
            minor,
            access: ACC_ALL,
        }
    }
}

/// Pseudo-devices every job must retain (systemd's `DevicePolicy=closed`): without
/// them a default-deny filter blocks `/dev/null` and ptys, breaking most programs.
pub fn base_device_rules() -> Vec<DeviceRule> {
    vec![
        DeviceRule::char_dev(1, Some(3)), // /dev/null
        DeviceRule::char_dev(1, Some(5)), // /dev/zero
        DeviceRule::char_dev(1, Some(7)), // /dev/full
        DeviceRule::char_dev(1, Some(8)), // /dev/random
        DeviceRule::char_dev(1, Some(9)), // /dev/urandom
        DeviceRule::char_dev(5, Some(0)), // /dev/tty
        DeviceRule::char_dev(5, Some(1)), // /dev/console
        DeviceRule::char_dev(5, Some(2)), // /dev/ptmx
        DeviceRule::char_dev(136, None),  // /dev/pts/*
    ]
}

/// Shared by every workload and owned by no allocation, so nothing puts them in a
/// job's `device_paths`; denying them breaks jobs not reaching for a GPU at all.
const HOST_INFRA_DEVICE_NODES: &[&str] = &[
    "/dev/fuse", // Apptainer/Singularity run inside the batch job
    // CUDA initialization; enumerating a GPU still needs /dev/nvidia<N>.
    "/dev/nvidiactl",
    "/dev/nvidia-uvm",
    "/dev/nvidia-uvm-tools",
    "/dev/nvidia-modeset",
];

/// Entry names are per-host (`uverbs0`, `nvidia-cap2`, ...), so these are enumerated.
///
/// `/dev/nvidia-caps` is granted wholesale only because a MIG instance is not
/// individually allocatable here; MIG support must move those behind allocation.
const HOST_INFRA_DEVICE_DIRS: &[&str] = &[
    "/dev/infiniband",  // RDMA verbs: MPI, NCCL and RCCL over IB
    "/dev/nvidia-caps", // MIG capability nodes
];

/// IB and NVIDIA majors are assigned dynamically, so these resolve by path like any
/// allocated node; a node without that hardware has nothing to stat.
///
/// Per-GPU compute nodes (`/dev/nvidia<N>`, `/dev/kfd`, `/dev/dri/*`) are
/// deliberately absent — gating those on the allocation is the security property.
pub fn host_infra_device_paths() -> Vec<String> {
    let mut paths: Vec<String> = HOST_INFRA_DEVICE_NODES
        .iter()
        .map(|p| (*p).to_string())
        .collect();
    for dir in HOST_INFRA_DEVICE_DIRS {
        paths.extend(device_dir_entries(Path::new(dir)));
    }
    paths
}

/// Sorted so the rule order is the same on every launch. A missing directory means
/// the node has no such hardware, which is not an error.
fn device_dir_entries(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.path().to_str().map(str::to_string))
        .collect();
    paths.sort();
    paths
}

/// Host infrastructure, the operator's additions, then the job's own allocation.
/// Each rule is an independent allow, so the order changes no verdict.
pub fn device_paths_for_job(allocated: &[String], extra: &[String]) -> Vec<String> {
    let mut paths = host_infra_device_paths();
    paths.extend(extra.iter().cloned());
    paths.extend(allocated.iter().cloned());
    paths
}

/// Base pseudo-devices plus the allocated nodes. Paths that do not stat to a device
/// node are skipped, so a zero-GPU job gets only the base rules.
pub fn rules_for_device_paths(
    paths: &[String],
    stat_dev: impl Fn(&str) -> Option<(DevType, u32, u32)>,
) -> Vec<DeviceRule> {
    let mut rules = base_device_rules();
    for path in paths {
        let Some((dev_type, major, minor)) = stat_dev(path) else {
            continue;
        };
        rules.push(DeviceRule {
            dev_type,
            major: Some(major),
            minor: Some(minor),
            access: ACC_ALL,
        });
    }
    rules
}

/// Production `stat_dev`: resolve a path to (type, major, minor) via `stat(2)`.
pub fn stat_device_node(path: &str) -> Option<(DevType, u32, u32)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let meta = std::fs::metadata(path).ok()?;
    let ft = meta.file_type();
    let dev_type = if ft.is_char_device() {
        DevType::Char
    } else if ft.is_block_device() {
        DevType::Block
    } else {
        return None;
    };
    let rdev = meta.rdev();
    let major = libc::major(rdev) as u32;
    let minor = libc::minor(rdev) as u32;
    Some((dev_type, major, minor))
}

/// The kernel `bpf_insn` ABI (uapi/linux/bpf.h) — loaded verbatim by `bpf()`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BpfInsn {
    pub code: u8,
    /// Destination register in the low nibble, source register in the high nibble.
    pub regs: u8,
    pub off: i16,
    pub imm: i32,
}

// The kernel loads these verbatim from a pointer plus an instruction count, so
// the 8-byte size is ABI, not an implementation detail.
const _: () = assert!(std::mem::size_of::<BpfInsn>() == 8);

// BPF_W and BPF_K are 0 but stay explicit so each opcode reads the way the kernel
// headers spell it.
const BPF_LDX: u8 = 0x01;
const BPF_ALU: u8 = 0x04;
const BPF_JMP: u8 = 0x05;
const BPF_W: u8 = 0x00;
const BPF_MEM: u8 = 0x60;
const BPF_K: u8 = 0x00;
const BPF_X: u8 = 0x08;
const BPF_AND: u8 = 0x50;
const BPF_RSH: u8 = 0x70;
const BPF_MOV: u8 = 0xb0;
const BPF_JNE: u8 = 0x50;
const BPF_EXIT: u8 = 0x90;

/// The only jump this module emits, so the offset patch pass can find every
/// mismatch branch by opcode alone.
const MISMATCH_JUMP: u8 = BPF_JMP | BPF_JNE | BPF_K;

const R0: u8 = 0;
const R1: u8 = 1; // ctx pointer on entry
const R2: u8 = 2; // device type
const R3: u8 = 3; // major
const R4: u8 = 4; // minor
const R5: u8 = 5; // requested access bits, live for every rule
const R6: u8 = 6; // scratch for the per-rule access test

// struct bpf_cgroup_dev_ctx. access_type packs the requested access bits above
// the device type, so one field feeds both R2 and R5.
const CTX_ACCESS_TYPE: i16 = 0;
const CTX_MAJOR: i16 = 4;
const CTX_MINOR: i16 = 8;
const DEV_TYPE_MASK: i32 = 0xffff;
const ACCESS_SHIFT: i32 = 16;

const DEVCG_DEV_BLOCK: i32 = 1;
const DEVCG_DEV_CHAR: i32 = 2;

const ALLOW: i32 = 1;
const DENY: i32 = 0;

fn regs(dst: u8, src: u8) -> u8 {
    (src << 4) | (dst & 0x0f)
}

fn ldx_w(dst: u8, src: u8, off: i16) -> BpfInsn {
    BpfInsn {
        code: BPF_LDX | BPF_MEM | BPF_W,
        regs: regs(dst, src),
        off,
        imm: 0,
    }
}

fn mov_imm(dst: u8, imm: i32) -> BpfInsn {
    BpfInsn {
        code: BPF_ALU | BPF_MOV | BPF_K,
        regs: regs(dst, 0),
        off: 0,
        imm,
    }
}

fn mov_reg(dst: u8, src: u8) -> BpfInsn {
    BpfInsn {
        code: BPF_ALU | BPF_MOV | BPF_X,
        regs: regs(dst, src),
        off: 0,
        imm: 0,
    }
}

fn and_imm(dst: u8, imm: i32) -> BpfInsn {
    BpfInsn {
        code: BPF_ALU | BPF_AND | BPF_K,
        regs: regs(dst, 0),
        off: 0,
        imm,
    }
}

fn rsh_imm(dst: u8, imm: i32) -> BpfInsn {
    BpfInsn {
        code: BPF_ALU | BPF_RSH | BPF_K,
        regs: regs(dst, 0),
        off: 0,
        imm,
    }
}

/// Mismatch branch with a placeholder offset; `rule_block` patches the target.
fn jne_imm(dst: u8, imm: i32) -> BpfInsn {
    BpfInsn {
        code: MISMATCH_JUMP,
        regs: regs(dst, 0),
        off: 0,
        imm,
    }
}

fn exit() -> BpfInsn {
    BpfInsn {
        code: BPF_JMP | BPF_EXIT,
        regs: 0,
        off: 0,
        imm: 0,
    }
}

/// Default-deny `cgroup/dev` program: allow the listed rules, deny everything else.
fn build_device_filter(rules: &[DeviceRule]) -> Vec<BpfInsn> {
    let mut prog = vec![
        // Both halves of access_type: R2 = device type, R5 = requested access.
        ldx_w(R2, R1, CTX_ACCESS_TYPE),
        and_imm(R2, DEV_TYPE_MASK),
        ldx_w(R5, R1, CTX_ACCESS_TYPE),
        rsh_imm(R5, ACCESS_SHIFT),
        ldx_w(R3, R1, CTX_MAJOR),
        ldx_w(R4, R1, CTX_MINOR),
    ];

    for rule in rules {
        prog.extend(rule_block(rule));
    }

    prog.push(mov_imm(R0, DENY));
    prog.push(exit());
    prog
}

/// One rule's compare-and-allow block. Any mismatch jumps to the instruction
/// just past the block, so the next rule — or the trailing deny — runs.
fn rule_block(rule: &DeviceRule) -> Vec<BpfInsn> {
    let type_imm = match rule.dev_type {
        DevType::Char => DEVCG_DEV_CHAR,
        DevType::Block => DEVCG_DEV_BLOCK,
    };

    let mut block = vec![jne_imm(R2, type_imm)];
    if let Some(major) = rule.major {
        block.push(jne_imm(R3, major as i32));
    }
    if let Some(minor) = rule.minor {
        block.push(jne_imm(R4, minor as i32));
    }
    // Subset test: `requested & !granted == 0`. The complement spans the whole
    // 16-bit access field, so an access bit a future kernel adds fails closed.
    block.push(mov_reg(R6, R5)); // scratch copy; R5 must survive for later rules
    block.push(and_imm(R6, i32::from(!u16::from(rule.access))));
    block.push(jne_imm(R6, 0));
    block.push(mov_imm(R0, ALLOW));
    block.push(exit());

    // A taken jump resumes at `i + off + 1`, so `off = len - 1 - i` sends every
    // mismatch to the first instruction after this block.
    let len = block.len();
    for (i, insn) in block.iter_mut().enumerate() {
        if insn.code == MISMATCH_JUMP {
            insn.off = (len - 1 - i) as i16;
        }
    }
    block
}

const BPF_PROG_LOAD: i32 = 5;
const BPF_PROG_ATTACH: i32 = 8;
const BPF_PROG_TYPE_CGROUP_DEVICE: u32 = 15;
const BPF_CGROUP_DEVICE: u32 = 6;

/// The kernel reads this as a C string; a GPL-compatible license is what every
/// in-tree device filter ships.
const BPF_LICENSE: &CStr = c"GPL";

/// Only allocated for the diagnostic retry after a failed load, so it can afford to
/// be generous — an overflowed log turns the real rejection into an opaque `ENOSPC`.
const VERIFIER_LOG_SIZE: usize = 64 * 1024;

/// The prefix of `union bpf_attr` that one `bpf(2)` command reads. `bpf()` passes
/// `size_of::<Self>()` and the kernel zero-fills the rest, so the tail is unmodeled.
///
/// # Safety
///
/// Implementors must be `#[repr(C)]` and padding-free, with every field at the
/// offset the kernel reads for that command, so writing them initializes all bytes.
unsafe trait BpfAttr {}

/// `BPF_PROG_LOAD`'s view of the union.
#[repr(C)]
#[derive(Default)]
struct ProgLoadAttr {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
}

// Offsets are ABI and getting one wrong surfaces only as `EINVAL`; the size
// assertion pins the absence of padding.
const _: () = {
    assert!(std::mem::align_of::<ProgLoadAttr>() == 8);
    assert!(std::mem::size_of::<ProgLoadAttr>() == 40);
    assert!(std::mem::offset_of!(ProgLoadAttr, prog_type) == 0);
    assert!(std::mem::offset_of!(ProgLoadAttr, insn_cnt) == 4);
    assert!(std::mem::offset_of!(ProgLoadAttr, insns) == 8);
    assert!(std::mem::offset_of!(ProgLoadAttr, license) == 16);
    assert!(std::mem::offset_of!(ProgLoadAttr, log_level) == 24);
    assert!(std::mem::offset_of!(ProgLoadAttr, log_size) == 28);
    assert!(std::mem::offset_of!(ProgLoadAttr, log_buf) == 32);
};

// SAFETY: `#[repr(C)]` at PROG_LOAD's offsets, both asserted above, and the
// asserted size leaves no room for padding.
unsafe impl BpfAttr for ProgLoadAttr {}

/// `BPF_PROG_ATTACH`'s view of the union. `union bpf_attr` is 8-byte aligned in the
/// uapi header, so the alignment is ABI even though every field here is 4 bytes.
#[repr(C, align(8))]
struct ProgAttachAttr {
    target_fd: u32,
    attach_bpf_fd: u32,
    attach_type: u32,
    attach_flags: u32,
}

const _: () = {
    assert!(std::mem::align_of::<ProgAttachAttr>() == 8);
    assert!(std::mem::size_of::<ProgAttachAttr>() == 16);
    assert!(std::mem::offset_of!(ProgAttachAttr, target_fd) == 0);
    assert!(std::mem::offset_of!(ProgAttachAttr, attach_bpf_fd) == 4);
    assert!(std::mem::offset_of!(ProgAttachAttr, attach_type) == 8);
    assert!(std::mem::offset_of!(ProgAttachAttr, attach_flags) == 12);
};

// SAFETY: as above, at PROG_ATTACH's offsets.
unsafe impl BpfAttr for ProgAttachAttr {}

/// Raw `bpf(2)`, handing the kernel `attr` as the command's `union bpf_attr`.
///
/// # Safety
///
/// `A` must be the attr type the kernel reads for `cmd`: the same bytes are
/// reinterpreted per command, so a mismatch can make it read a flag as a pointer.
///
/// Every pointer field in `attr` must be valid and stay live for the call — the
/// kernel dereferences them directly and nothing ties them to `attr`.
unsafe fn bpf<A: BpfAttr>(cmd: i32, attr: &mut A) -> libc::c_long {
    // SAFETY: `attr` is uniquely borrowed for the call and padding-free per
    // `BpfAttr`, so its bytes are initialized; the caller guarantees the buffers.
    unsafe {
        libc::syscall(
            libc::SYS_bpf,
            cmd,
            attr as *mut A,
            std::mem::size_of::<A>() as libc::c_uint,
        )
    }
}

/// `log` is supplied only on the retry after a failure: with a log attached the
/// verifier traces every instruction and fails the load if the trace does not fit.
fn try_prog_load(prog: &[BpfInsn], log: Option<&mut [u8]>) -> std::io::Result<OwnedFd> {
    // Derived here rather than passed in: the kernel sizes its read through `insns`
    // entirely from `insn_cnt`, so pointer and length cannot disagree.
    let insn_cnt = u32::try_from(prog.len())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut attr = ProgLoadAttr {
        prog_type: BPF_PROG_TYPE_CGROUP_DEVICE,
        insn_cnt,
        insns: prog.as_ptr() as u64,
        license: BPF_LICENSE.as_ptr() as u64,
        ..ProgLoadAttr::default()
    };
    if let Some(log) = log {
        // `log_size` is 32 bits of ABI: refuse a buffer that does not fit rather
        // than silently handing the kernel a truncated size for it.
        let log_size = u32::try_from(log.len())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        attr.log_level = 1;
        attr.log_size = log_size;
        attr.log_buf = log.as_mut_ptr() as u64;
    }

    // SAFETY: `ProgLoadAttr` is BPF_PROG_LOAD's union member and `insn_cnt` comes
    // from `prog.len()`; `prog`, `log` and the `'static` license all outlive the call.
    let fd = unsafe { bpf(BPF_PROG_LOAD, &mut attr) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `bpf()` just created this descriptor for our process and returned
    // sole ownership of it, so nothing else will close it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Trim the kernel's NUL-terminated verifier log down to the text it wrote.
fn verifier_log_text(buf: &[u8]) -> String {
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).trim_end().to_string()
}

/// The load is gated on `bpf_capable()`, so both paths need the capability hint;
/// `other_cause` names whatever else returns `EPERM`, since the errno cannot.
fn eperm_hint(err: &std::io::Error, other_cause: Option<&str>) -> String {
    if err.raw_os_error() != Some(libc::EPERM) {
        return String::new();
    }
    match other_cause {
        Some(cause) => format!(" (spurd needs CAP_BPF or CAP_SYS_ADMIN, or {cause})"),
        None => " (spurd needs CAP_BPF or CAP_SYS_ADMIN)".to_string(),
    }
}

fn bpf_prog_load(prog: &[BpfInsn]) -> anyhow::Result<OwnedFd> {
    let insn_cnt = u32::try_from(prog.len()).context("device filter is too large to load")?;
    let err = match try_prog_load(prog, None) {
        Ok(fd) => return Ok(fd),
        Err(e) => e,
    };

    // The verifier's rejection message is the only diagnostic a real node gets.
    // Discarding the retry is safe: a surprise success drops its fd unattached.
    let mut log = vec![0u8; VERIFIER_LOG_SIZE];
    drop(try_prog_load(prog, Some(&mut log)));
    let log = verifier_log_text(&log);
    // No `other_cause`: the exclusive-attach conflict is an attach-time verdict,
    // and naming it here would send the reader after the wrong thing.
    let hint = eperm_hint(&err, None);
    // `E2BIG` from exceeding BPF_MAXINSNS short-circuits before the verifier
    // runs, so the count is the only clue that the rule list is what was too big.
    if log.is_empty() {
        anyhow::bail!("BPF_PROG_LOAD of {insn_cnt} instructions failed: {err}{hint}");
    }
    anyhow::bail!(
        "BPF_PROG_LOAD of {insn_cnt} instructions failed: {err}{hint}; verifier log: {log}"
    )
}

/// `attach_flags` stays zero: an exclusive attach is the stricter contract for a
/// security filter, and it is what makes this the job cgroup's only device program.
fn prog_attach_attr(prog_fd: RawFd, cgroup_fd: RawFd) -> ProgAttachAttr {
    ProgAttachAttr {
        target_fd: cgroup_fd as u32,
        attach_bpf_fd: prog_fd as u32,
        attach_type: BPF_CGROUP_DEVICE,
        attach_flags: 0,
    }
}

/// Typed owners rather than `RawFd`s so transposing them is a compile error and
/// neither descriptor can be closed while the syscall is in flight.
fn bpf_prog_attach(prog_fd: &OwnedFd, cgroup_dir: &File) -> anyhow::Result<()> {
    let mut attr = prog_attach_attr(prog_fd.as_raw_fd(), cgroup_dir.as_raw_fd());
    // SAFETY: `ProgAttachAttr` is BPF_PROG_ATTACH's union member and holds only
    // descriptors and flags, so the kernel dereferences nothing.
    if unsafe { bpf(BPF_PROG_ATTACH, &mut attr) } < 0 {
        let err = std::io::Error::last_os_error();
        // The two causes need opposite responses: grant a capability, or relax the
        // ancestor.
        let hint = eperm_hint(
            &err,
            Some("an ancestor cgroup already holds an exclusive device program"),
        );
        anyhow::bail!("BPF_PROG_ATTACH failed: {err}{hint}");
    }
    Ok(())
}

/// Compile `rules` into a default-deny device filter and attach it to `cgroup_dir`.
///
/// Both descriptors drop before returning, leaving the attachment the program's only
/// owner: removing the cgroup at teardown frees it, so there is nothing to detach.
pub fn install_device_filter(cgroup_dir: &Path, rules: &[DeviceRule]) -> anyhow::Result<()> {
    let prog = build_device_filter(rules);
    let prog_fd = bpf_prog_load(&prog)?;
    let cgroup_fd = File::open(cgroup_dir)
        .with_context(|| format!("open cgroup dir {}", cgroup_dir.display()))?;
    bpf_prog_attach(&prog_fd, &cgroup_fd)
        .with_context(|| format!("attach device filter to {}", cgroup_dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_rules_include_standard_pseudo_devices() {
        let rules = base_device_rules();
        // Exact minors, so no single wildcard rule can satisfy several of these
        // at once and hide a rule set that is missing devices.
        let has = |maj: u32, min: u32| {
            rules.iter().any(|r| {
                matches!(r.dev_type, DevType::Char) && r.major == Some(maj) && r.minor == Some(min)
            })
        };
        assert!(has(1, 3), "/dev/null");
        assert!(has(1, 5), "/dev/zero");
        assert!(has(1, 8), "/dev/random");
        assert!(has(1, 9), "/dev/urandom");
        assert!(has(5, 0), "/dev/tty");
        assert!(has(5, 2), "/dev/ptmx");
        assert!(
            rules.iter().any(|r| matches!(r.dev_type, DevType::Char)
                && r.major == Some(136)
                && r.minor.is_none()),
            "/dev/pts/* must be a wildcard minor: pty numbers are allocated at runtime"
        );
    }

    #[test]
    fn adds_rules_for_allocated_device_nodes_only() {
        let paths = vec![
            "/dev/kfd".to_string(),
            "/dev/dri/renderD128".to_string(),
            "/dev/nvme0n1".to_string(),
            "/dev/does-not-exist".to_string(),
        ];
        let fake_stat = |p: &str| match p {
            "/dev/kfd" => Some((DevType::Char, 234u32, 0u32)),
            "/dev/dri/renderD128" => Some((DevType::Char, 226u32, 128u32)),
            "/dev/nvme0n1" => Some((DevType::Block, 259u32, 0u32)),
            _ => None,
        };
        let rules = rules_for_device_paths(&paths, fake_stat);
        assert!(
            rules
                .iter()
                .any(|r| r.major == Some(1) && r.minor == Some(3)),
            "base /dev/null rule retained"
        );
        assert!(
            rules
                .iter()
                .any(|r| r.major == Some(234) && r.minor == Some(0)),
            "/dev/kfd allowed"
        );
        assert!(
            rules
                .iter()
                .any(|r| r.major == Some(226) && r.minor == Some(128)),
            "/dev/dri/renderD128 allowed"
        );
        assert_eq!(
            rules.iter().filter(|r| r.major == Some(226)).count(),
            1,
            "the allocated render node adds exactly one rule"
        );
        assert_eq!(
            &rules[base_device_rules().len()..],
            &[
                DeviceRule {
                    dev_type: DevType::Char,
                    major: Some(234),
                    minor: Some(0),
                    access: ACC_ALL,
                },
                DeviceRule {
                    dev_type: DevType::Char,
                    major: Some(226),
                    minor: Some(128),
                    access: ACC_ALL,
                },
                DeviceRule {
                    dev_type: DevType::Block,
                    major: Some(259),
                    minor: Some(0),
                    access: ACC_ALL,
                },
            ],
            "only the resolvable paths add rules, in input order, keeping each node's type"
        );
    }

    #[test]
    fn zero_gpu_job_gets_only_base_rules() {
        let rules = rules_for_device_paths(&[], |_| None);
        assert_eq!(rules, base_device_rules());
    }

    #[test]
    fn host_infra_list_covers_the_shared_nodes_no_allocation_grants() {
        let paths = host_infra_device_paths();
        for expected in [
            "/dev/fuse",
            "/dev/nvidiactl",
            "/dev/nvidia-uvm",
            "/dev/nvidia-uvm-tools",
            "/dev/nvidia-modeset",
        ] {
            assert!(
                paths.iter().any(|p| p == expected),
                "{expected} is host infrastructure, not an allocatable device"
            );
        }
        assert!(
            HOST_INFRA_DEVICE_DIRS.contains(&"/dev/infiniband"),
            "RDMA verbs nodes must be enumerated; a non-GPU MPI job needs them"
        );
        assert!(
            HOST_INFRA_DEVICE_DIRS.contains(&"/dev/nvidia-caps"),
            "MIG capability nodes must be enumerated"
        );
    }

    /// The whole point of the filter is that a compute node needs an allocation.
    /// Listing one here would hand every job on the node a GPU.
    #[test]
    fn host_infra_list_excludes_per_gpu_compute_nodes() {
        let paths = host_infra_device_paths();
        for forbidden in [
            "/dev/kfd",
            "/dev/nvidia0",
            "/dev/dri",
            "/dev/dri/renderD128",
        ] {
            assert!(
                !paths.iter().any(|p| p == forbidden),
                "{forbidden} is allocation-gated and must not be granted unconditionally"
            );
        }
        for dir in HOST_INFRA_DEVICE_DIRS {
            assert_ne!(*dir, "/dev/dri", "render/card nodes stay allocation-gated");
        }
    }

    #[test]
    fn device_dir_entries_lists_a_directorys_nodes_and_tolerates_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("uverbs1"), b"").unwrap();
        std::fs::write(dir.path().join("uverbs0"), b"").unwrap();

        let entries = device_dir_entries(dir.path());
        let names: Vec<&str> = entries
            .iter()
            .filter_map(|p| p.rsplit('/').next())
            .collect();
        assert_eq!(
            names,
            ["uverbs0", "uverbs1"],
            "sorted for a stable rule order"
        );

        assert_eq!(
            device_dir_entries(&dir.path().join("absent")),
            Vec::<String>::new(),
            "a node without that hardware has no entries, not an error"
        );
    }

    #[test]
    fn operator_extra_paths_become_rules() {
        let extra = vec!["/dev/site-accel0".to_string()];
        let paths = device_paths_for_job(&[], &extra);
        assert!(
            paths.iter().any(|p| p == "/dev/site-accel0"),
            "extra_device_paths must reach the resolver"
        );

        let rules = rules_for_device_paths(&paths, |p| match p {
            "/dev/site-accel0" => Some((DevType::Char, 511, 3)),
            _ => None,
        });
        assert_eq!(
            &rules[base_device_rules().len()..],
            &[DeviceRule {
                dev_type: DevType::Char,
                major: Some(511),
                minor: Some(3),
                access: ACC_ALL,
            }],
            "an operator's device is allowed without declaring it as GRES"
        );
    }

    #[test]
    fn allocated_paths_survive_alongside_host_infra_and_extras() {
        let allocated = vec!["/dev/dri/renderD128".to_string()];
        let extra = vec!["/dev/site-accel0".to_string()];
        let paths = device_paths_for_job(&allocated, &extra);
        assert!(paths.iter().any(|p| p == "/dev/dri/renderD128"));
        assert!(paths.iter().any(|p| p == "/dev/site-accel0"));
        assert!(paths.iter().any(|p| p == "/dev/nvidiactl"));
    }

    #[test]
    fn acc_constants_match_kernel_devcg_values() {
        assert_eq!(ACC_MKNOD, 1, "BPF_DEVCG_ACC_MKNOD");
        assert_eq!(ACC_READ, 2, "BPF_DEVCG_ACC_READ");
        assert_eq!(ACC_WRITE, 4, "BPF_DEVCG_ACC_WRITE");
        assert_eq!(ACC_ALL, 7, "read | write | mknod");
    }

    #[test]
    fn base_rules_grant_full_access() {
        for rule in base_device_rules() {
            assert_eq!(
                rule.access, ACC_ALL,
                "{rule:?} must grant read/write/mknod on a pseudo-device"
            );
        }
    }

    /// Synthetic `struct bpf_cgroup_dev_ctx`. The packing is spelled out from the
    /// kernel ABI, not the codegen constants, so the layout is pinned independently.
    #[derive(Clone, Copy)]
    struct DevCtx {
        access_type: u32,
        major: u32,
        minor: u32,
    }

    fn dev_ctx(dev_type: DevType, access: u8, major: u32, minor: u32) -> DevCtx {
        let type_bits: u32 = match dev_type {
            DevType::Block => 1,
            DevType::Char => 2,
        };
        DevCtx {
            access_type: (u32::from(access) << 16) | type_bits,
            major,
            minor,
        }
    }

    /// Sentinel ctx address: every load must be derived from the pointer in R1,
    /// so clobbering R1 makes the interpreter fault instead of reading the ctx.
    const CTX_BASE: u64 = 0x1000;
    const STEP_LIMIT: usize = 4096;

    /// Runs the generated instruction stream rather than a hand-rolled encoding. An
    /// unhandled opcode panics: a mis-encoded always-allow program would look healthy.
    fn run_filter(prog: &[BpfInsn], ctx: DevCtx) -> u32 {
        // Re-derived from the kernel headers rather than the module's constants, so
        // a typo cannot make codegen and interpreter agree on a wrong encoding.
        const LDX_W: u8 = 0x61; // BPF_LDX | BPF_MEM | BPF_W
        const MOV_IMM: u8 = 0xb4; // BPF_ALU | BPF_MOV | BPF_K
        const MOV_REG: u8 = 0xbc; // BPF_ALU | BPF_MOV | BPF_X
        const AND_IMM: u8 = 0x54; // BPF_ALU | BPF_AND | BPF_K
        const RSH_IMM: u8 = 0x74; // BPF_ALU | BPF_RSH | BPF_K
        const JNE_IMM: u8 = 0x55; // BPF_JMP | BPF_JNE | BPF_K
        const EXIT: u8 = 0x95; // BPF_JMP | BPF_EXIT

        let mut regs = [0u64; 11];
        regs[1] = CTX_BASE;
        let mut pc = 0usize;
        for _ in 0..STEP_LIMIT {
            let insn = *prog
                .get(pc)
                .unwrap_or_else(|| panic!("pc {pc} ran past the end of the program"));
            let dst = usize::from(insn.regs & 0x0f);
            let src = usize::from(insn.regs >> 4);
            let at = pc;
            pc += 1;
            match insn.code {
                LDX_W => {
                    let addr = regs[src].wrapping_add_signed(i64::from(insn.off));
                    regs[dst] = u64::from(match addr.wrapping_sub(CTX_BASE) {
                        0 => ctx.access_type,
                        4 => ctx.major,
                        8 => ctx.minor,
                        _ => panic!("insn {at} loads outside struct bpf_cgroup_dev_ctx"),
                    });
                }
                MOV_IMM => regs[dst] = u64::from(insn.imm as u32),
                MOV_REG => regs[dst] = u64::from(regs[src] as u32),
                AND_IMM => regs[dst] = u64::from(regs[dst] as u32 & insn.imm as u32),
                RSH_IMM => regs[dst] = u64::from((regs[dst] as u32) >> (insn.imm as u32 & 31)),
                JNE_IMM => {
                    if regs[dst] != i64::from(insn.imm) as u64 {
                        pc = pc.wrapping_add_signed(isize::from(insn.off));
                    }
                }
                EXIT => return regs[0] as u32,
                code => panic!("insn {at}: unsupported opcode {code:#04x}"),
            }
        }
        panic!("program did not reach EXIT within {STEP_LIMIT} steps");
    }

    /// Allow-list for a job allocated exactly one device node, char 234:0.
    fn one_gpu_rules() -> Vec<DeviceRule> {
        rules_for_device_paths(&["/dev/kfd".to_string()], |_| Some((DevType::Char, 234, 0)))
    }

    #[test]
    fn filter_allows_an_allocated_device_node() {
        let prog = build_device_filter(&one_gpu_rules());
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_READ, 234, 0)),
            1,
            "the job's own /dev/kfd must be readable"
        );
    }

    #[test]
    fn filter_denies_a_device_the_job_was_not_allocated() {
        let prog = build_device_filter(&one_gpu_rules());
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_READ, 226, 129)),
            0,
            "another job's render node must be denied"
        );
    }

    #[test]
    fn filter_allows_base_pseudo_devices() {
        let prog = build_device_filter(&base_device_rules());
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_READ | ACC_WRITE, 1, 3)),
            1,
            "/dev/null must stay read/write"
        );
    }

    #[test]
    fn filter_distinguishes_block_from_char_devices() {
        let prog = build_device_filter(&base_device_rules());
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Block, ACC_READ, 1, 3)),
            0,
            "block 1:3 must not inherit the char 1:3 (/dev/null) rule"
        );
    }

    /// Pins where a mismatch jump resumes: overshooting by one would skip the
    /// next rule's device-type check, undershooting would exit the block early.
    #[test]
    fn a_mismatched_rule_resumes_at_the_next_rules_type_check() {
        let rules = vec![
            DeviceRule {
                dev_type: DevType::Char,
                major: Some(99),
                minor: Some(0),
                access: ACC_ALL,
            },
            DeviceRule {
                dev_type: DevType::Block,
                major: Some(234),
                minor: Some(0),
                access: ACC_ALL,
            },
        ];
        let prog = build_device_filter(&rules);
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_READ, 234, 0)),
            0,
            "char 234:0 must not be allowed by the block rule reached after rule 1 mismatched"
        );
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Block, ACC_READ, 234, 0)),
            1,
            "block 234:0 is allowed, so a rule-1 mismatch must still reach rule 2"
        );
    }

    #[test]
    fn filter_honors_wildcard_minor_rules() {
        let prog = build_device_filter(&base_device_rules());
        for minor in [0, 4095] {
            assert_eq!(
                run_filter(
                    &prog,
                    dev_ctx(DevType::Char, ACC_READ | ACC_WRITE, 136, minor)
                ),
                1,
                "/dev/pts/{minor} must match the wildcard-minor rule"
            );
        }
    }

    #[test]
    fn filter_treats_requested_access_as_a_subset_test() {
        let read_only = DeviceRule {
            dev_type: DevType::Char,
            major: Some(234),
            minor: Some(0),
            access: ACC_READ,
        };
        let prog = build_device_filter(&[read_only]);
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_READ, 234, 0)),
            1,
            "read is granted"
        );
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_WRITE, 234, 0)),
            0,
            "write is not granted"
        );
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_MKNOD, 234, 0)),
            0,
            "mknod is not granted"
        );
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_READ | ACC_WRITE, 234, 0)),
            0,
            "a request must be a subset of the rule, not merely overlap it"
        );
    }

    /// Masking the complement to the three bits that exist today would make an
    /// unknown bit AND to zero and be allowed by every rule.
    #[test]
    fn filter_denies_an_access_bit_no_rule_can_grant() {
        const UNKNOWN_ACC: u8 = 8; // one past ACC_WRITE
        let prog = build_device_filter(&base_device_rules());
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_READ, 1, 3)),
            1,
            "/dev/null is readable, so the deny below is about the extra bit"
        );
        assert_eq!(
            run_filter(&prog, dev_ctx(DevType::Char, ACC_READ | UNKNOWN_ACC, 1, 3)),
            0,
            "an access bit outside ACC_* must fail closed even on an ACC_ALL rule"
        );
    }

    #[test]
    fn filter_with_no_rules_denies_everything() {
        let prog = build_device_filter(&[]);
        for ctx in [
            dev_ctx(DevType::Char, ACC_READ, 1, 3),
            dev_ctx(DevType::Char, ACC_READ, 234, 0),
            dev_ctx(DevType::Block, ACC_WRITE, 8, 0),
        ] {
            assert_eq!(
                run_filter(&prog, ctx),
                0,
                "an empty allow-list must deny every device"
            );
        }
    }

    #[test]
    fn program_is_nonempty_and_deny_by_default() {
        let prog = build_device_filter(&[]);
        assert!(prog.len() >= 2, "must at least load-deny-and-exit");
        let last = prog.last().copied().unwrap();
        assert_eq!(last.code, BPF_JMP | BPF_EXIT, "program must end with EXIT");
        let deny = prog[prog.len() - 2];
        assert_eq!(
            deny.code,
            BPF_ALU | BPF_MOV | BPF_K,
            "deny must be a MOV imm"
        );
        assert_eq!(deny.imm, 0, "the fall-through must return 0 (deny)");
    }

    #[test]
    fn more_rules_produce_more_instructions() {
        let few = build_device_filter(&base_device_rules());
        let many = build_device_filter(&one_gpu_rules());
        assert!(
            many.len() > few.len(),
            "each allocated node adds compare instructions"
        );
    }

    #[test]
    fn every_mismatch_jump_lands_inside_the_program() {
        let rules = one_gpu_rules();
        let prog = build_device_filter(&rules);
        let mut jumps = 0;
        for (i, insn) in prog.iter().enumerate() {
            if insn.code != MISMATCH_JUMP {
                continue;
            }
            jumps += 1;
            let target = i as i64 + i64::from(insn.off) + 1;
            assert!(
                target > i as i64 && target < prog.len() as i64,
                "jump at {i} targets {target}, outside the program body"
            );
        }
        assert!(
            jumps >= rules.len(),
            "every rule must emit at least one mismatch jump; found {jumps} for {} rules",
            rules.len()
        );
    }

    #[test]
    fn stat_device_node_rejects_paths_that_are_not_device_nodes() {
        let dir = tempfile::tempdir().unwrap();

        let regular = dir.path().join("regular-file");
        std::fs::write(&regular, b"not a device").unwrap();
        assert_eq!(
            stat_device_node(regular.to_str().unwrap()),
            None,
            "regular file is not a device node"
        );

        let missing = dir.path().join("does-not-exist");
        assert_eq!(
            stat_device_node(missing.to_str().unwrap()),
            None,
            "nonexistent path is not a device node"
        );
    }

    /// Both descriptors are `RawFd`, so only this pins which one is the attach
    /// target; the offset assertions cover where each lands in the union.
    #[test]
    fn attach_targets_the_cgroup_with_the_program_as_the_attached_fd() {
        let attr = prog_attach_attr(7, 11);
        assert_eq!(attr.target_fd, 11, "the cgroup is what is attached to");
        assert_eq!(attr.attach_bpf_fd, 7, "the program is what is attached");
        assert_eq!(attr.attach_type, BPF_CGROUP_DEVICE);
        assert_eq!(
            attr.attach_flags, 0,
            "no BPF_F_ALLOW_MULTI: one exclusive filter per job cgroup"
        );
    }

    /// The load path is where the hint is actually read, and there it must not send
    /// anyone hunting for an ancestor cgroup that has nothing to do with it.
    #[test]
    fn eperm_names_the_capability_on_every_path_and_the_ancestor_only_on_attach() {
        let eperm = std::io::Error::from_raw_os_error(libc::EPERM);

        let load = eperm_hint(&eperm, None);
        assert!(load.contains("CAP_BPF"), "load hint names the capability");
        assert!(
            !load.contains("ancestor"),
            "the exclusive-attach conflict cannot cause a failed load: {load}"
        );

        let attach = eperm_hint(&eperm, Some("an ancestor cgroup already holds it"));
        assert!(attach.contains("CAP_BPF"));
        assert!(attach.contains("an ancestor cgroup already holds it"));

        assert_eq!(
            eperm_hint(&std::io::Error::from_raw_os_error(libc::EINVAL), None),
            "",
            "a capability hint on EINVAL would misdirect"
        );
    }

    #[test]
    fn verifier_log_stops_at_the_kernels_terminator() {
        let mut buf = vec![0u8; 64];
        buf[..14].copy_from_slice(b"invalid insn\n\0");
        assert_eq!(verifier_log_text(&buf), "invalid insn");
        assert_eq!(
            verifier_log_text(&[0u8; 64]),
            "",
            "an unwritten log is empty"
        );
        assert_eq!(
            verifier_log_text(b"no terminator"),
            "no terminator",
            "a full buffer is still readable"
        );
    }
}
