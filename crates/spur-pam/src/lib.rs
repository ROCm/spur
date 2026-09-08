// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#![cfg(target_os = "linux")]
#![deny(unsafe_op_in_unsafe_fn)]

use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

const PAM_SUCCESS: c_int = 0;
const PAM_SYSTEM_ERR: c_int = 4;
const PAM_PERM_DENIED: c_int = 6;
const PAM_SERVICE: c_int = 1;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE: usize = 4096;

extern "C" {
    fn pam_get_user(pamh: *mut c_void, user: *mut *const c_char, prompt: *const c_char) -> c_int;
    fn pam_get_item(pamh: *const c_void, item: c_int, value: *mut *const c_void) -> c_int;
    fn pam_putenv(pamh: *mut c_void, assignment: *const c_char) -> c_int;
}

type PamResult<T> = Result<T, c_int>;

#[derive(Debug, PartialEq, Eq)]
struct Allocation {
    job_id: String,
    gpu_csv: String,
}

fn valid_username(user: &[u8]) -> bool {
    !user.is_empty()
        && user.len() <= 128
        && user != b"root"
        && user
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(c))
}

fn decimal(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn parse_response(line: &[u8]) -> PamResult<Allocation> {
    if line.len() > MAX_RESPONSE || !line.is_ascii() || !line.ends_with(b"\n") {
        return Err(PAM_SYSTEM_ERR);
    }
    if line == b"DENY\n" {
        return Err(PAM_PERM_DENIED);
    }
    let text = std::str::from_utf8(&line[..line.len() - 1]).map_err(|_| PAM_SYSTEM_ERR)?;
    let mut fields = text.split(' ');
    if fields.next() != Some("OK") {
        return Err(PAM_SYSTEM_ERR);
    }
    let job_id = fields.next().ok_or(PAM_SYSTEM_ERR)?;
    let gpu_csv = fields.next().ok_or(PAM_SYSTEM_ERR)?;
    if fields.next().is_some() || !matches!(decimal(job_id), Some(1..)) {
        return Err(PAM_SYSTEM_ERR);
    }
    if gpu_csv != "-1" {
        let mut seen = std::collections::HashSet::new();
        for gpu in gpu_csv.split(',') {
            let index = decimal(gpu).ok_or(PAM_SYSTEM_ERR)?;
            if index > c_int::MAX as u64 || !seen.insert(index) {
                return Err(PAM_SYSTEM_ERR);
            }
        }
    }
    Ok(Allocation {
        job_id: job_id.to_owned(),
        gpu_csv: gpu_csv.to_owned(),
    })
}

fn require_root(uid: libc::uid_t) -> PamResult<()> {
    if uid == 0 {
        Ok(())
    } else {
        Err(PAM_PERM_DENIED)
    }
}

fn check_peer(stream: &UnixStream, expected_uid: libc::uid_t) -> PamResult<()> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(PAM_SYSTEM_ERR);
    }
    if credentials.uid != expected_uid || credentials.pid <= 0 {
        return Err(PAM_PERM_DENIED);
    }
    Ok(())
}

fn daemon_uid() -> libc::uid_t {
    #[cfg(not(test))]
    {
        0
    }
    #[cfg(test)]
    {
        tests::EXPECTED_UID.with(|uid| uid.get())
    }
}

fn caller_uid() -> libc::uid_t {
    #[cfg(not(test))]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(test)]
    {
        tests::CALLER_UID.with(|uid| uid.get())
    }
}

fn remaining(deadline: Instant) -> PamResult<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(PAM_SYSTEM_ERR)
}

fn wait_connected(stream: &UnixStream, deadline: Instant) -> PamResult<()> {
    loop {
        let budget = remaining(deadline)?;
        let millis = budget.as_millis() + u128::from(budget.subsec_nanos() % 1_000_000 != 0);
        let timeout = c_int::try_from(millis).unwrap_or(c_int::MAX);
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: descriptor is valid for one pollfd and stream owns its open descriptor.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        remaining(deadline)?;
        if result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(PAM_SYSTEM_ERR);
        }
        if result == 0 {
            continue;
        }
        if descriptor.revents & libc::POLLNVAL != 0
            || stream.take_error().map_err(|_| PAM_SYSTEM_ERR)?.is_some()
            || descriptor.revents & libc::POLLOUT == 0
        {
            return Err(PAM_SYSTEM_ERR);
        }
        return Ok(());
    }
}

fn connect(path: &Path, deadline: Instant) -> PamResult<UnixStream> {
    remaining(deadline)?;
    let bytes = path.as_os_str().as_bytes();
    let mut address = libc::sockaddr_un {
        sun_family: libc::AF_UNIX as libc::sa_family_t,
        sun_path: [0; 108],
    };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(PAM_SYSTEM_ERR);
    }
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as c_char;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    let length = libc::socklen_t::try_from(length).map_err(|_| PAM_SYSTEM_ERR)?;
    // SAFETY: AF_UNIX stream socket creation has no pointer arguments.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(PAM_SYSTEM_ERR);
    }
    // SAFETY: fd is a new, valid stream socket whose sole ownership is transferred here.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    remaining(deadline)?;
    // SAFETY: address is initialized and length includes only its bounded, NUL-terminated path.
    let result = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if result != 0 {
        // Linux AF_UNIX EAGAIN means a full queue, not a pending connection.
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(PAM_SYSTEM_ERR);
        }
        wait_connected(&stream, deadline)?;
    }
    remaining(deadline)?;
    stream.set_nonblocking(false).map_err(|_| PAM_SYSTEM_ERR)?;
    Ok(stream)
}

fn write_request(stream: &mut UnixStream, mut request: &[u8], deadline: Instant) -> PamResult<()> {
    while !request.is_empty() {
        stream
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|_| PAM_SYSTEM_ERR)?;
        match stream.write(request) {
            Ok(0) => return Err(PAM_SYSTEM_ERR),
            Ok(written) => request = &request[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(PAM_SYSTEM_ERR),
        }
    }
    remaining(deadline)?;
    Ok(())
}

fn read_response(stream: &mut UnixStream, deadline: Instant) -> PamResult<Allocation> {
    let mut line = Vec::with_capacity(128);
    loop {
        // An absolute deadline also bounds a peer that trickles bytes indefinitely.
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|_| PAM_SYSTEM_ERR)?;
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(1) => {
                remaining(deadline)?;
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    return parse_response(&line);
                }
                if line.len() >= MAX_RESPONSE {
                    return Err(PAM_SYSTEM_ERR);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            _ => return Err(PAM_SYSTEM_ERR),
        }
    }
}

fn receive_membership(stream: &UnixStream, deadline: Instant) -> PamResult<OwnedFd> {
    const MAX_FDS: usize = 8;
    let control_size = unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<c_int>()) as u32) } as usize;
    // cmsghdr storage guarantees alignment, unlike a byte array.
    let mut control = vec![
        unsafe { std::mem::zeroed::<libc::cmsghdr>() };
        control_size.div_ceil(size_of::<libc::cmsghdr>())
    ];
    loop {
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|_| PAM_SYSTEM_ERR)?;
        let mut byte = [0u8];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control_size;
        // One byte preserves the following stream response; CLOEXEC is atomic with receipt.
        let received =
            unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
        if received < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(PAM_SYSTEM_ERR);
        }
        let mut descriptors = Vec::with_capacity(MAX_FDS);
        let mut malformed = message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0;
        let mut rights_messages = 0;
        let header_size = unsafe { libc::CMSG_LEN(0) } as usize;
        let mut offset = 0;
        let length = message.msg_controllen.min(control_size);
        malformed |= message.msg_controllen > control_size;
        while length.saturating_sub(offset) >= size_of::<libc::cmsghdr>() {
            // The kernel supplies aligned headers, but unaligned reads also bound malformed input.
            let base = unsafe { message.msg_control.cast::<u8>().add(offset) };
            let header = unsafe { ptr::read_unaligned(base.cast::<libc::cmsghdr>()) };
            if header.cmsg_len < header_size {
                malformed = true;
                break;
            }
            let available = length - offset;
            let bounded_length = header.cmsg_len.min(available);
            malformed |= header.cmsg_len > available;
            if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
                rights_messages += 1;
                let payload = bounded_length.saturating_sub(header_size);
                malformed |= payload == 0 || payload % size_of::<c_int>() != 0;
                for index in 0..payload / size_of::<c_int>() {
                    let fd = unsafe {
                        ptr::read_unaligned(
                            base.add(header_size + index * size_of::<c_int>()).cast(),
                        )
                    };
                    if fd < 0 {
                        malformed = true;
                    } else {
                        // Own every delivered descriptor before any error/deadline return.
                        descriptors.push(unsafe { OwnedFd::from_raw_fd(fd) });
                    }
                }
            } else {
                malformed = true;
            }
            if header.cmsg_len > available {
                break;
            }
            let aligned_length = header.cmsg_len.div_ceil(size_of::<usize>()) * size_of::<usize>();
            offset += aligned_length;
        }
        remaining(deadline)?;
        if malformed || received != 1 {
            return Err(PAM_SYSTEM_ERR);
        }
        if byte[0] == b'D' && descriptors.is_empty() && rights_messages == 0 {
            return Err(PAM_PERM_DENIED);
        }
        if byte[0] != b'F' || descriptors.len() != 1 || rights_messages != 1 {
            return Err(PAM_SYSTEM_ERR);
        }
        let fd = descriptors.pop().ok_or(PAM_SYSTEM_ERR)?;
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        let descriptor_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if flags < 0
            || !matches!(flags & libc::O_ACCMODE, libc::O_WRONLY | libc::O_RDWR)
            || descriptor_flags < 0
            || descriptor_flags & libc::FD_CLOEXEC == 0
            || unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0
        {
            return Err(PAM_SYSTEM_ERR);
        }
        if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(PAM_SYSTEM_ERR);
        }
        return Ok(fd);
    }
}

fn join_membership(fd: OwnedFd, deadline: Instant) -> PamResult<()> {
    // A single cgroup write of zero adopts this caller, never a reusable numeric PID.
    loop {
        remaining(deadline)?;
        let written = unsafe { libc::write(fd.as_raw_fd(), b"0\n".as_ptr().cast(), 2) };
        if written == 2 {
            break;
        }
        if written < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {
            continue;
        }
        return Err(PAM_SYSTEM_ERR);
    }
    drop(fd);
    remaining(deadline)?;
    Ok(())
}

fn exchange(path: &Path, user: &str, adopt: bool) -> PamResult<Allocation> {
    let deadline = Instant::now() + IO_TIMEOUT;
    // Never cache this connection: ADOPT's SO_PEERCRED must identify the session caller.
    let mut stream = connect(path, deadline)?;
    check_peer(&stream, daemon_uid())?;
    let operation = if adopt { "ADOPT" } else { "CHECK" };
    write_request(
        &mut stream,
        format!("SPURSSH1 {operation} {user}\n").as_bytes(),
        deadline,
    )?;
    if adopt {
        let fd = receive_membership(&stream, deadline)?;
        join_membership(fd, deadline)?;
        write_request(&mut stream, b"JOINED\n", deadline)?;
    }
    read_response(&mut stream, deadline)
}

unsafe fn context<'a>(
    pamh: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
) -> PamResult<(&'a Path, &'a str)> {
    if pamh.is_null() || argc != 1 || argv.is_null() {
        return Err(PAM_SYSTEM_ERR);
    }
    require_root(caller_uid())?;
    let option = unsafe { *argv };
    if option.is_null() {
        return Err(PAM_SYSTEM_ERR);
    }
    let option = unsafe { CStr::from_ptr(option) }.to_bytes();
    let socket = option.strip_prefix(b"socket=").ok_or(PAM_SYSTEM_ERR)?;
    let path = Path::new(std::ffi::OsStr::from_bytes(socket));
    if !path.is_absolute() {
        return Err(PAM_SYSTEM_ERR);
    }
    let mut service = ptr::null();
    let status = unsafe { pam_get_item(pamh, PAM_SERVICE, &mut service) };
    if status != PAM_SUCCESS {
        return Err(status);
    }
    if service.is_null() || unsafe { CStr::from_ptr(service.cast()) }.to_bytes() != b"sshd" {
        return Err(PAM_PERM_DENIED);
    }
    let mut user = ptr::null();
    let status = unsafe { pam_get_user(pamh, &mut user, ptr::null()) };
    if status != PAM_SUCCESS {
        return Err(status);
    }
    if user.is_null() {
        return Err(PAM_PERM_DENIED);
    }
    let user = unsafe { CStr::from_ptr(user) }.to_bytes();
    if !valid_username(user) {
        return Err(PAM_PERM_DENIED);
    }
    Ok((
        path,
        std::str::from_utf8(user).map_err(|_| PAM_PERM_DENIED)?,
    ))
}

unsafe fn export_allocation(pamh: *mut c_void, allocation: Allocation) -> PamResult<()> {
    // Only allocation metadata is replaced; sshd/PAM retain HOME, SHELL and login context.
    for (key, value) in [
        ("SPUR_JOB_ID", &allocation.job_id),
        ("SLURM_JOB_ID", &allocation.job_id),
        ("ROCR_VISIBLE_DEVICES", &allocation.gpu_csv),
        ("CUDA_VISIBLE_DEVICES", &allocation.gpu_csv),
        ("GPU_DEVICE_ORDINAL", &allocation.gpu_csv),
    ] {
        let assignment = CString::new(format!("{key}={value}")).map_err(|_| PAM_SYSTEM_ERR)?;
        let status = unsafe { pam_putenv(pamh, assignment.as_ptr()) };
        if status != PAM_SUCCESS {
            return Err(status);
        }
    }
    Ok(())
}

fn boundary(action: impl FnOnce() -> PamResult<()>) -> c_int {
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(Ok(())) => PAM_SUCCESS,
        Ok(Err(status)) => status,
        Err(_) => PAM_SYSTEM_ERR,
    }
}

/// # Safety
/// Arguments must satisfy the Linux-PAM module ABI, including valid C strings.
#[no_mangle]
pub unsafe extern "C" fn pam_sm_acct_mgmt(
    pamh: *mut c_void,
    _flags: c_int,
    argc: c_int,
    argv: *const *const c_char,
) -> c_int {
    boundary(|| {
        let (path, user) = unsafe { context(pamh, argc, argv) }?;
        exchange(path, user, false)?;
        Ok(())
    })
}

/// # Safety
/// Arguments must satisfy the Linux-PAM module ABI, including valid C strings.
#[no_mangle]
pub unsafe extern "C" fn pam_sm_open_session(
    pamh: *mut c_void,
    _flags: c_int,
    argc: c_int,
    argv: *const *const c_char,
) -> c_int {
    boundary(|| {
        let (path, user) = unsafe { context(pamh, argc, argv) }?;
        let allocation = exchange(path, user, true)?;
        unsafe { export_allocation(pamh, allocation) }
    })
}

/// # Safety
/// Arguments must satisfy the Linux-PAM module ABI, including valid C strings.
#[no_mangle]
pub unsafe extern "C" fn pam_sm_close_session(
    pamh: *mut c_void,
    _flags: c_int,
    argc: c_int,
    argv: *const *const c_char,
) -> c_int {
    boundary(|| {
        unsafe { context(pamh, argc, argv) }?;
        // Allocation lifetime belongs to the scheduler, not any individual SSH session.
        Ok(())
    })
}

#[cfg(test)]
mod tests;
