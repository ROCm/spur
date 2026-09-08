// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

thread_local! {
    pub(super) static EXPECTED_UID: Cell<libc::uid_t> = Cell::new(unsafe { libc::geteuid() });
    pub(super) static CALLER_UID: Cell<libc::uid_t> = const { Cell::new(0) };
}

struct FakePam {
    user: Option<CString>,
    service: Option<CString>,
    user_error: c_int,
    item_error: c_int,
    env_error: c_int,
    env: BTreeMap<String, String>,
}

impl FakePam {
    fn new() -> Self {
        Self {
            user: Some(CString::new("alice").unwrap()),
            service: Some(CString::new("sshd").unwrap()),
            user_error: 0,
            item_error: 0,
            env_error: 0,
            env: BTreeMap::from([
                ("HOME".into(), "/home/alice".into()),
                ("SHELL".into(), "/bin/bash".into()),
                ("USER".into(), "alice".into()),
                ("LOGNAME".into(), "alice".into()),
                ("CUDA_VISIBLE_DEVICES".into(), "untrusted".into()),
                ("SPUR_JOB_ID".into(), "untrusted".into()),
            ]),
        }
    }

    fn handle(&mut self) -> *mut c_void {
        (self as *mut Self).cast()
    }
}

#[no_mangle]
unsafe extern "C" fn pam_get_user(
    pamh: *mut c_void,
    user: *mut *const c_char,
    _prompt: *const c_char,
) -> c_int {
    let pam = unsafe { &mut *pamh.cast::<FakePam>() };
    unsafe { *user = pam.user.as_ref().map_or(ptr::null(), |s| s.as_ptr()) };
    pam.user_error
}

#[no_mangle]
unsafe extern "C" fn pam_get_item(
    pamh: *const c_void,
    item: c_int,
    value: *mut *const c_void,
) -> c_int {
    if item != PAM_SERVICE {
        return PAM_SYSTEM_ERR;
    }
    let pam = unsafe { &*pamh.cast::<FakePam>() };
    unsafe {
        *value = pam
            .service
            .as_ref()
            .map_or(ptr::null(), |s| s.as_ptr().cast())
    };
    pam.item_error
}

#[no_mangle]
unsafe extern "C" fn pam_putenv(pamh: *mut c_void, assignment: *const c_char) -> c_int {
    let pam = unsafe { &mut *pamh.cast::<FakePam>() };
    if pam.env_error != 0 {
        return pam.env_error;
    }
    let Ok(text) = (unsafe { CStr::from_ptr(assignment) }).to_str() else {
        return PAM_SYSTEM_ERR;
    };
    let Some((key, value)) = text.split_once('=') else {
        return PAM_SYSTEM_ERR;
    };
    pam.env.insert(key.into(), value.into());
    PAM_SUCCESS
}

struct SocketPath(std::path::PathBuf);
impl SocketPath {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        Self(std::env::temp_dir().join(format!(
            "spur-pam-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn option(&self) -> CString {
        CString::new(format!("socket={}", self.0.display())).unwrap()
    }
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

type Entry = unsafe extern "C" fn(*mut c_void, c_int, c_int, *const *const c_char) -> c_int;

fn call(entry: Entry, pam: &mut FakePam, options: &[&str]) -> c_int {
    let strings: Vec<_> = options.iter().map(|s| CString::new(*s).unwrap()).collect();
    let args: Vec<_> = strings.iter().map(|s| s.as_ptr()).collect();
    unsafe { entry(pam.handle(), 0, args.len() as c_int, args.as_ptr()) }
}

fn request(stream: &mut UnixStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        assert!(bytes.len() < 256);
    }
    String::from_utf8(bytes).unwrap()
}

fn membership_file() -> (SocketPath, std::fs::File) {
    let path = SocketPath::new();
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path.0)
        .unwrap();
    (path, file)
}

fn send_fds(mut stream: &UnixStream, bytes: &[u8], fds: &[c_int]) {
    assert!(!bytes.is_empty());
    if fds.is_empty() {
        stream.write_all(bytes).unwrap();
        return;
    }
    let payload = std::mem::size_of_val(fds);
    let control_size = unsafe { libc::CMSG_SPACE(payload as u32) } as usize;
    let mut control = vec![
        unsafe { std::mem::zeroed::<libc::cmsghdr>() };
        control_size.div_ceil(size_of::<libc::cmsghdr>())
    ];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control_size;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(payload as u32) as usize;
        ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(header), payload);
        assert_eq!(
            libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL),
            bytes.len() as isize
        );
    }
}

fn handoff(stream: &mut UnixStream) {
    let (path, file) = membership_file();
    send_fds(stream, b"F", &[file.as_raw_fd()]);
    assert_eq!(request(stream), "JOINED\n");
    assert_eq!(references_to(&file), 1);
    assert_eq!(std::fs::read(&path.0).unwrap(), b"0\n");
}

fn serve_once(entry: Entry, pam: &mut FakePam, response: Vec<u8>) -> (c_int, String) {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let line = request(&mut stream);
        if line.starts_with("SPURSSH1 ADOPT ") && response.starts_with(b"OK ") {
            handoff(&mut stream);
        }
        let _ = stream.write_all(&response);
        line
    });
    let result = call(entry, pam, &[path.option().to_str().unwrap()]);
    (result, worker.join().unwrap())
}

#[test]
fn exported_check_then_adopt_connect_separately_and_preserve_login_context() {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let worker = thread::spawn(move || {
        let (mut monitor, _) = listener.accept().unwrap();
        assert_eq!(request(&mut monitor), "SPURSSH1 CHECK alice\n");
        monitor.write_all(b"OK 21 0\n").unwrap();
        assert_eq!(monitor.read(&mut [0]).unwrap(), 0);
        let (mut session, _) = listener.accept().unwrap();
        check_peer(&session, unsafe { libc::geteuid() }).unwrap();
        assert_eq!(request(&mut session), "SPURSSH1 ADOPT alice\n");
        handoff(&mut session);
        session.write_all(b"OK 42 2,5\n").unwrap();
    });
    let mut pam = FakePam::new();
    let before = pam.env.clone();
    let option = path.option();
    assert_eq!(
        call(pam_sm_acct_mgmt, &mut pam, &[option.to_str().unwrap()]),
        PAM_SUCCESS
    );
    assert_eq!(pam.env, before);
    assert_eq!(
        call(pam_sm_open_session, &mut pam, &[option.to_str().unwrap()]),
        PAM_SUCCESS
    );
    worker.join().unwrap();
    for key in ["SPUR_JOB_ID", "SLURM_JOB_ID"] {
        assert_eq!(pam.env[key], "42");
    }
    for key in [
        "ROCR_VISIBLE_DEVICES",
        "CUDA_VISIBLE_DEVICES",
        "GPU_DEVICE_ORDINAL",
    ] {
        assert_eq!(pam.env[key], "2,5");
    }
    for key in ["HOME", "SHELL", "USER", "LOGNAME"] {
        assert_eq!(pam.env[key], before[key]);
    }
    let adopted = pam.env.clone();
    // The listener is gone: close must neither contact it nor release anything.
    assert_eq!(
        call(pam_sm_close_session, &mut pam, &[option.to_str().unwrap()]),
        PAM_SUCCESS
    );
    assert_eq!(pam.env, adopted);
}

#[test]
fn valid_responses_and_no_gpu_mask() {
    for line in [
        b"OK 1 0\n".as_slice(),
        b"OK 18446744073709551615 0,2,17\n",
        b"OK 42 -1\n",
    ] {
        assert!(parse_response(line).is_ok());
    }
    let mut pam = FakePam::new();
    assert_eq!(
        serve_once(pam_sm_open_session, &mut pam, b"OK 8 -1\n".to_vec()).0,
        0
    );
    for key in [
        "ROCR_VISIBLE_DEVICES",
        "CUDA_VISIBLE_DEVICES",
        "GPU_DEVICE_ORDINAL",
    ] {
        assert_eq!(pam.env[key], "-1");
    }
}

#[test]
fn parser_rejects_malformed_and_oversized_responses() {
    for line in [
        "",
        "DENY",
        "DENY \n",
        "OK 1 0",
        "OK 1 0\r\n",
        "OK 1 0\nDENY\n",
        "OK 1 0 extra\n",
        "OK  1 0\n",
        "OK\t1 0\n",
        "OK 0 0\n",
        "OK -1 0\n",
        "OK +1 0\n",
        "OK 18446744073709551616 0\n",
        "OK 1 \n",
        "OK 1 -2\n",
        "OK 1 -1,0\n",
        "OK 1 1,\n",
        "OK 1 ,1\n",
        "OK 1 0,,1\n",
        "OK 1 a\n",
        "OK 1 0,0\n",
        "OK 1 0,00\n",
        "OK 1 2147483648\n",
        "OK 1 0=evil\n",
        "OK 1 0\0\n",
        "OK 1 é\n",
        "ok 1 0\n",
    ] {
        assert_eq!(
            parse_response(line.as_bytes()),
            Err(PAM_SYSTEM_ERR),
            "{line:?}"
        );
    }
    assert_eq!(
        parse_response(&[b'x'; MAX_RESPONSE + 1]),
        Err(PAM_SYSTEM_ERR)
    );
    assert_eq!(parse_response(b"DENY\n"), Err(PAM_PERM_DENIED));
}

#[test]
fn bounded_socket_reader_handles_fragmentation_eof_and_oversize() {
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        for part in [b"OK ".as_slice(), b"7", b" 1,3", b"\n"] {
            writer.write_all(part).unwrap();
        }
    });
    assert_eq!(
        read_response(&mut reader, Instant::now() + IO_TIMEOUT)
            .unwrap()
            .gpu_csv,
        "1,3"
    );
    worker.join().unwrap();
    for data in [b"OK 1 0".to_vec(), vec![], vec![b'x'; MAX_RESPONSE + 1]] {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&data).unwrap();
        drop(writer);
        assert_eq!(
            read_response(&mut reader, Instant::now() + IO_TIMEOUT),
            Err(PAM_SYSTEM_ERR)
        );
    }
}

#[test]
fn validates_username_exact_ascii_bounds() {
    for user in ["a", "Alice_09.foo-bar", ".", "ROOT"] {
        assert!(valid_username(user.as_bytes()));
    }
    assert!(valid_username(&[b'a'; 128]));
    assert!(!valid_username(&[b'a'; 129]));
    for user in [
        "", "root", "a b", "alice\n", "alice/", "é", "a=1", "a\t", "a\0",
    ] {
        assert!(!valid_username(user.as_bytes()), "{user:?}");
    }
}

#[test]
fn exported_abi_rejects_bad_context_and_options() {
    for entry in [
        pam_sm_acct_mgmt as Entry,
        pam_sm_open_session,
        pam_sm_close_session,
    ] {
        for options in [
            vec![],
            vec!["socket=relative"],
            vec!["socket="],
            vec!["debug"],
            vec!["socket=/tmp/test", "permissive"],
            vec!["socket=/a", "socket=/b"],
        ] {
            assert_eq!(call(entry, &mut FakePam::new(), &options), PAM_SYSTEM_ERR);
        }
        let arg = CString::new("socket=/unused").unwrap();
        assert_eq!(
            unsafe { entry(ptr::null_mut(), 0, 1, &arg.as_ptr()) },
            PAM_SYSTEM_ERR
        );
        assert_eq!(
            unsafe { entry(FakePam::new().handle(), 0, 1, ptr::null()) },
            PAM_SYSTEM_ERR
        );
        assert_eq!(
            unsafe { entry(FakePam::new().handle(), 0, 1, &ptr::null()) },
            PAM_SYSTEM_ERR
        );
        assert_eq!(
            unsafe { entry(FakePam::new().handle(), 0, -1, &arg.as_ptr()) },
            PAM_SYSTEM_ERR
        );
        for service in [None, Some("login"), Some("SSHD"), Some("sshd ")] {
            let mut pam = FakePam::new();
            pam.service = service.map(|s| CString::new(s).unwrap());
            assert_eq!(call(entry, &mut pam, &["socket=/unused"]), PAM_PERM_DENIED);
        }
        for user in [None, Some("root"), Some("bad\nuser"), Some("")] {
            let mut pam = FakePam::new();
            pam.user = user.map(|s| CString::new(s).unwrap());
            assert_eq!(call(entry, &mut pam, &["socket=/unused"]), PAM_PERM_DENIED);
        }
        CALLER_UID.with(|uid| uid.set(1000));
        let result = call(entry, &mut FakePam::new(), &["socket=/unused"]);
        CALLER_UID.with(|uid| uid.set(0));
        assert_eq!(result, PAM_PERM_DENIED);
    }
    assert_eq!(require_root(0), Ok(()));
    assert_eq!(require_root(1000), Err(PAM_PERM_DENIED));
}

#[test]
fn pam_and_daemon_errors_fail_closed() {
    for entry in [pam_sm_acct_mgmt as Entry, pam_sm_open_session] {
        let mut pam = FakePam::new();
        pam.item_error = 3;
        assert_eq!(call(entry, &mut pam, &["socket=/unused"]), 3);
        pam.item_error = 0;
        pam.user_error = 10;
        assert_eq!(call(entry, &mut pam, &["socket=/unused"]), 10);
        pam.user_error = 0;
        let path = SocketPath::new();
        assert_eq!(
            call(entry, &mut pam, &[path.option().to_str().unwrap()]),
            PAM_SYSTEM_ERR
        );
        for (response, expected) in [
            (b"DENY\n".to_vec(), PAM_PERM_DENIED),
            (b"garbage\n".to_vec(), PAM_SYSTEM_ERR),
            (vec![], PAM_SYSTEM_ERR),
            (vec![b'x'; MAX_RESPONSE + 1], PAM_SYSTEM_ERR),
        ] {
            let before = pam.env.clone();
            assert_eq!(serve_once(entry, &mut pam, response).0, expected);
            assert_eq!(pam.env, before);
        }
    }
    let mut pam = FakePam::new();
    pam.env_error = 5;
    assert_eq!(
        serve_once(pam_sm_open_session, &mut pam, b"OK 1 0\n".to_vec()).0,
        5
    );
}

#[test]
fn kernel_peer_credentials_reject_wrong_owner_before_sending_username() {
    let uid = unsafe { libc::geteuid() };
    let (stream, _other) = UnixStream::pair().unwrap();
    assert_eq!(check_peer(&stream, uid), Ok(()));
    assert_eq!(
        check_peer(&stream, uid.wrapping_add(1)),
        Err(PAM_PERM_DENIED)
    );
    for entry in [pam_sm_acct_mgmt as Entry, pam_sm_open_session] {
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.0).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        EXPECTED_UID.with(|expected| expected.set(uid.wrapping_add(1)));
        let result = call(
            entry,
            &mut FakePam::new(),
            &[path.option().to_str().unwrap()],
        );
        EXPECTED_UID.with(|expected| expected.set(uid));
        assert_eq!(result, PAM_PERM_DENIED);
        worker.join().unwrap();
    }
}

#[test]
fn connect_rejects_missing_refused_and_invalid_paths() {
    let path = SocketPath::new();
    assert_eq!(
        UnixStream::connect(&path.0).unwrap_err().raw_os_error(),
        Some(libc::ENOENT)
    );
    assert_eq!(
        connect(&path.0, Instant::now() + IO_TIMEOUT).unwrap_err(),
        PAM_SYSTEM_ERR
    );
    let listener = UnixListener::bind(&path.0).unwrap();
    drop(listener);
    assert_eq!(
        UnixStream::connect(&path.0).unwrap_err().raw_os_error(),
        Some(libc::ECONNREFUSED)
    );
    assert_eq!(
        connect(&path.0, Instant::now() + IO_TIMEOUT).unwrap_err(),
        PAM_SYSTEM_ERR
    );
    for bytes in [
        vec![b'x'; 108],
        vec![b'x'; 4096],
        vec![],
        b"/tmp/a\0b".to_vec(),
    ] {
        let path = Path::new(std::ffi::OsStr::from_bytes(&bytes));
        assert_eq!(
            connect(path, Instant::now() + IO_TIMEOUT).unwrap_err(),
            PAM_SYSTEM_ERR
        );
    }
}

#[test]
fn full_backlog_fails_closed_and_connected_socket_is_blocking_cloexec() {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    // SAFETY: listener owns a valid listening socket; zero gives one pending slot on Linux.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let stream = connect(&path.0, Instant::now() + IO_TIMEOUT).unwrap();
    // SAFETY: F_GETFL and F_GETFD inspect a valid descriptor without additional arguments.
    let status_flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
    let descriptor_flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) };
    assert!(status_flags >= 0);
    assert_eq!(status_flags & libc::O_NONBLOCK, 0);
    assert!(descriptor_flags >= 0);
    assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
    assert_eq!(
        connect(&path.0, Instant::now() + IO_TIMEOUT).unwrap_err(),
        PAM_SYSTEM_ERR
    );
    let (_accepted, _) = listener.accept().unwrap();
    assert!(connect(&path.0, Instant::now() + IO_TIMEOUT).is_ok());
}

#[test]
fn expired_deadline_is_shared_by_connect_poll_write_and_read() {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now();
    assert_eq!(connect(&path.0, deadline).unwrap_err(), PAM_SYSTEM_ERR);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let (mut stream, mut peer) = UnixStream::pair().unwrap();
    assert_eq!(wait_connected(&stream, deadline), Err(PAM_SYSTEM_ERR));
    assert_eq!(
        write_request(&mut stream, b"SPURSSH1 CHECK alice\n", deadline),
        Err(PAM_SYSTEM_ERR)
    );
    peer.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.read(&mut [0]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    peer.write_all(b"OK 1 0\n").unwrap();
    assert_eq!(read_response(&mut stream, deadline), Err(PAM_SYSTEM_ERR));
    let mut untouched = [0; 7];
    stream.read_exact(&mut untouched).unwrap();
    assert_eq!(&untouched, b"OK 1 0\n");
}

#[test]
fn connection_poll_checks_socket_error() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    assert_eq!(wait_connected(&stream, Instant::now() + IO_TIMEOUT), Ok(()));
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let stream = connect(&path.0, Instant::now() + IO_TIMEOUT).unwrap();
    drop(listener);
    assert_eq!(
        wait_connected(&stream, Instant::now() + IO_TIMEOUT),
        Err(PAM_SYSTEM_ERR)
    );
    assert!(stream.take_error().unwrap().is_none());
}

#[test]
fn membership_fd_is_cloexec_and_receipt_consumes_only_one_byte() {
    let (path, file) = membership_file();
    let (mut reader, writer) = UnixStream::pair().unwrap();
    send_fds(&writer, b"FOK 9 1\n", &[file.as_raw_fd()]);
    let fd = receive_membership(&reader, Instant::now() + IO_TIMEOUT).unwrap();
    assert_ne!(
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    assert!(std::fs::read(&path.0).unwrap().is_empty());
    join_membership(fd, Instant::now() + IO_TIMEOUT).unwrap();
    assert_eq!(std::fs::read(&path.0).unwrap(), b"0\n");
    assert_eq!(
        read_response(&mut reader, Instant::now() + IO_TIMEOUT)
            .unwrap()
            .job_id,
        "9"
    );
}

fn references_to(file: &std::fs::File) -> usize {
    use std::os::unix::fs::MetadataExt;
    let expected = file.metadata().unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::metadata(entry.path()).ok())
        .filter(|metadata| metadata.dev() == expected.dev() && metadata.ino() == expected.ino())
        .count()
}

#[test]
fn invalid_membership_handoffs_fail_closed_without_descriptor_leaks() {
    let (path, file) = membership_file();
    let readonly = std::fs::File::open(&path.0).unwrap();
    let path_c = CString::new(path.0.as_os_str().as_bytes()).unwrap();
    let path_fd = unsafe { libc::open(path_c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    assert!(path_fd >= 0);
    let path_fd = unsafe { OwnedFd::from_raw_fd(path_fd) };
    let initial = references_to(&file);
    for (byte, fds, expected) in [
        (b'F', vec![], PAM_SYSTEM_ERR),
        (b'X', vec![file.as_raw_fd()], PAM_SYSTEM_ERR),
        (b'D', vec![], PAM_PERM_DENIED),
        (b'D', vec![file.as_raw_fd()], PAM_SYSTEM_ERR),
        (b'F', vec![file.as_raw_fd(); 2], PAM_SYSTEM_ERR),
        (b'F', vec![file.as_raw_fd(); 16], PAM_SYSTEM_ERR),
        (b'F', vec![readonly.as_raw_fd()], PAM_SYSTEM_ERR),
        (b'F', vec![path_fd.as_raw_fd()], PAM_SYSTEM_ERR),
    ] {
        let (reader, writer) = UnixStream::pair().unwrap();
        send_fds(&writer, &[byte], &fds);
        assert_eq!(
            receive_membership(&reader, Instant::now() + IO_TIMEOUT).unwrap_err(),
            expected
        );
        // Count only this unique inode, so concurrently running tests cannot perturb it.
        assert_eq!(references_to(&file), initial);
        assert!(std::fs::read(&path.0).unwrap().is_empty());
    }
}

#[test]
fn membership_rejects_pipe_socket_and_directory() {
    let mut pipe = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let _pipe_reader = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
    let pipe_writer = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
    let (socket, _peer) = UnixStream::pair().unwrap();
    let directory = std::fs::File::open(std::env::temp_dir()).unwrap();
    for fd in [
        pipe_writer.as_raw_fd(),
        socket.as_raw_fd(),
        directory.as_raw_fd(),
    ] {
        let (reader, writer) = UnixStream::pair().unwrap();
        send_fds(&writer, b"F", &[fd]);
        assert_eq!(
            receive_membership(&reader, Instant::now() + IO_TIMEOUT).unwrap_err(),
            PAM_SYSTEM_ERR
        );
    }
}

#[test]
fn final_ack_failure_never_exports_environment() {
    for response in [b"DENY\n".as_slice(), b"JOINED\n", b"OK 0 1\n", b""] {
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.0).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(request(&mut stream), "SPURSSH1 ADOPT alice\n");
            handoff(&mut stream);
            stream.write_all(response).unwrap();
        });
        let mut pam = FakePam::new();
        let before = pam.env.clone();
        let result = call(
            pam_sm_open_session,
            &mut pam,
            &[path.option().to_str().unwrap()],
        );
        assert_eq!(
            result,
            if response == b"DENY\n" {
                PAM_PERM_DENIED
            } else {
                PAM_SYSTEM_ERR
            }
        );
        assert_eq!(pam.env, before);
        worker.join().unwrap();
    }
}

#[test]
fn check_never_writes_membership_even_if_peer_sends_rights() {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let (membership_path, file) = membership_file();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(request(&mut stream), "SPURSSH1 CHECK alice\n");
        send_fds(&stream, b"OK 7 2\n", &[file.as_raw_fd()]);
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        assert_eq!(references_to(&file), 1);
    });
    let mut pam = FakePam::new();
    let before = pam.env.clone();
    assert_eq!(
        call(
            pam_sm_acct_mgmt,
            &mut pam,
            &[path.option().to_str().unwrap()]
        ),
        PAM_SUCCESS
    );
    worker.join().unwrap();
    assert_eq!(pam.env, before);
    assert!(std::fs::read(&membership_path.0).unwrap().is_empty());
}

#[test]
fn membership_deadline_closes_fd_without_writing() {
    let (path, file) = membership_file();
    let (reader, writer) = UnixStream::pair().unwrap();
    send_fds(&writer, b"F", &[file.as_raw_fd()]);
    assert_eq!(
        receive_membership(&reader, Instant::now()).unwrap_err(),
        PAM_SYSTEM_ERR
    );
    let fd = receive_membership(&reader, Instant::now() + IO_TIMEOUT).unwrap();
    assert_eq!(references_to(&file), 2);
    assert_eq!(join_membership(fd, Instant::now()), Err(PAM_SYSTEM_ERR));
    assert_eq!(references_to(&file), 1);
    assert!(std::fs::read(&path.0).unwrap().is_empty());
}

#[test]
fn panic_boundary_returns_system_error() {
    assert_eq!(boundary(|| panic!("injected failure")), PAM_SYSTEM_ERR);
    assert_eq!(boundary(|| Err(PAM_PERM_DENIED)), PAM_PERM_DENIED);
    assert_eq!(boundary(|| Ok(())), PAM_SUCCESS);
}
