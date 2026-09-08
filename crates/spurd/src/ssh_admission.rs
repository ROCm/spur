// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;
use tonic::metadata::{Ascii, MetadataValue};

use crate::agent_server::RunningJobs;
use crate::job_lifecycle::JobLifecycle;
use crate::ssh_identity::IdentityResolver;
use spur_proto::proto::{GetJobRequest, JobInfo, JobState};

const MAX_REQUEST: usize = 256;
const STRICT_HEADER: &str = "x-spur-ssh-admission";

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub job_id: u32,
    pub user: String,
    pub uid: u32,
    pub run_attempt: u32,
    pub generation: Arc<()>,
    pub cgroup: PathBuf,
    pub gpu_devices: Vec<u32>,
}

#[derive(Clone)]
pub(crate) struct LocalJobs {
    pub running: RunningJobs,
    pub lifecycle: JobLifecycle,
}

fn unique_snapshot(
    jobs: &std::collections::HashMap<u32, crate::agent_server::TrackedJob>,
    user: &str,
    uid: u32,
) -> anyhow::Result<Snapshot> {
    let mut matches = jobs
        .iter()
        .filter_map(|(&id, job)| job.ssh_snapshot(id, user, uid));
    let snapshot = matches.next().context("no eligible job")?;
    ensure!(matches.next().is_none(), "ambiguous allocation");
    ensure!(snapshot.job_id != 0, "invalid job id");
    ensure!(snapshot.cgroup.is_dir(), "missing cgroup");
    Ok(snapshot)
}

impl LocalJobs {
    async fn snapshot(&self, user: &str, uid: u32) -> anyhow::Result<Snapshot> {
        unique_snapshot(&*self.running.lock().await, user, uid)
    }

    async fn finish(
        &self,
        snapshot: &Snapshot,
        controller: &JobInfo,
        hostname: &str,
        stream: &mut UnixStream,
        adopt: bool,
    ) -> anyhow::Result<()> {
        let _lifecycle = self.lifecycle.acquire(snapshot.job_id).await;
        let jobs = self.running.lock().await;
        let current = unique_snapshot(&jobs, &snapshot.user, snapshot.uid)?;
        ensure!(
            Arc::ptr_eq(&current.generation, &snapshot.generation)
                && current.cgroup == snapshot.cgroup,
            "allocation replaced"
        );
        validate_controller(controller, &current, hostname, SystemTime::now())?;
        let gpu_csv = if current.gpu_devices.is_empty() {
            "-1".to_owned()
        } else {
            let mut seen = std::collections::HashSet::new();
            ensure!(
                current
                    .gpu_devices
                    .iter()
                    .all(|&id| id <= i32::MAX as u32 && seen.insert(id)),
                "invalid GPU allocation"
            );
            current
                .gpu_devices
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        let response = format!("OK {} {gpu_csv}\n", current.job_id);
        ensure!(response.len() <= 4096, "allocation response too large");
        drop(jobs);
        if adopt {
            let file = open_membership(&current.cgroup)?;
            send_membership_fd(stream, std::os::fd::AsFd::as_fd(&file)).await?;
            drop(file);
            let mut receipt = [0; 7];
            stream.read_exact(&mut receipt).await?;
            ensure!(&receipt == b"JOINED\n", "invalid adoption receipt");
            let peer = stream.peer_cred()?;
            let pid = peer.pid().context("missing peer PID")?;
            verify_membership(&current.cgroup, pid)?;
            validate_controller(controller, &current, hostname, SystemTime::now())?;
        }
        let jobs = self.running.lock().await;
        let final_snapshot = unique_snapshot(&jobs, &snapshot.user, snapshot.uid)?;
        ensure!(
            Arc::ptr_eq(&final_snapshot.generation, &snapshot.generation)
                && final_snapshot.cgroup == snapshot.cgroup,
            "allocation replaced during adoption"
        );
        validate_controller(controller, &final_snapshot, hostname, SystemTime::now())?;
        drop(jobs);
        // The per-job lifecycle guard prevents teardown without blocking other allocations.
        stream.write_all(response.as_bytes()).await?;
        Ok(())
    }
}

fn valid_username(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 128
        && user != "root"
        && user
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}

fn parse_request(line: &[u8]) -> anyhow::Result<(bool, &str)> {
    ensure!(
        line.len() <= MAX_REQUEST && line.is_ascii(),
        "invalid request"
    );
    let text = std::str::from_utf8(line)?
        .strip_suffix('\n')
        .context("missing newline")?;
    let mut fields = text.split(' ');
    ensure!(fields.next() == Some("SPURSSH1"), "unknown protocol");
    let adopt = match fields.next() {
        Some("CHECK") => false,
        Some("ADOPT") => true,
        _ => bail!("unknown operation"),
    };
    let user = fields.next().context("missing username")?;
    ensure!(
        fields.next().is_none() && valid_username(user),
        "invalid username"
    );
    Ok((adopt, user))
}

fn validate_peer(uid: u32, pid: Option<i32>, expected_uid: u32) -> anyhow::Result<i32> {
    ensure!(uid == expected_uid, "non-root peer");
    let pid = pid.context("missing peer PID")?;
    ensure!(pid > 1, "invalid peer PID");
    Ok(pid)
}

fn validate_controller(
    info: &JobInfo,
    snapshot: &Snapshot,
    hostname: &str,
    now: SystemTime,
) -> anyhow::Result<()> {
    ensure!(
        info.job_id == snapshot.job_id
            && info.uid == snapshot.uid
            && info.user == snapshot.user
            && info.run_attempt == Some(snapshot.run_attempt)
            && info.state == JobState::JobRunning as i32
            && info.exclusive
            && info.end_time.is_none(),
        "controller allocation not eligible"
    );
    ensure!(
        spur_core::hostlist::expand(&info.nodelist)?
            .iter()
            .any(|node| node == hostname),
        "node not allocated"
    );
    let start = info.start_time.as_ref().context("missing start time")?;
    ensure!(
        start.seconds >= 0 && (0..1_000_000_000).contains(&start.nanos),
        "invalid start time"
    );
    let start = UNIX_EPOCH
        .checked_add(Duration::new(start.seconds as u64, start.nanos as u32))
        .context("start time overflow")?;
    ensure!(start <= now, "future start time");
    if let Some(limit) = &info.time_limit {
        ensure!(
            limit.seconds >= 0 && (0..1_000_000_000).contains(&limit.nanos),
            "invalid time limit"
        );
        let limit = Duration::new(limit.seconds as u64, limit.nanos as u32);
        ensure!(!limit.is_zero(), "zero time limit");
        let deadline = start.checked_add(limit).context("deadline overflow")?;
        ensure!(now < deadline, "allocation expired");
    }
    Ok(())
}

fn open_membership(cgroup: &Path) -> anyhow::Result<fs::File> {
    // No create or truncate: a removed cgroup must never become a regular file.
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(cgroup.join("cgroup.procs"))?;
    ensure!(file.metadata()?.is_file(), "invalid membership file");
    Ok(file)
}

async fn send_membership_fd(stream: &UnixStream, fd: BorrowedFd<'_>) -> io::Result<()> {
    loop {
        stream.writable().await?;
        let result = stream.try_io(Interest::WRITABLE, || {
            let mut marker = b'F';
            let mut iov = libc::iovec {
                iov_base: std::ptr::addr_of_mut!(marker).cast(),
                iov_len: 1,
            };
            // cmsghdr storage provides the alignment required by CMSG_*.
            let mut control = [unsafe { std::mem::zeroed::<libc::cmsghdr>() }; 2];
            let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen =
                unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as usize;
            assert!(message.msg_controllen <= std::mem::size_of_val(&control));
            // All pointers refer to live, aligned storage through this synchronous syscall.
            let sent = unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len =
                    libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as usize;
                libc::CMSG_DATA(header)
                    .cast::<libc::c_int>()
                    .write(fd.as_raw_fd());
                libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL)
            };
            match sent {
                1 => Ok(()),
                -1 => Err(io::Error::last_os_error()),
                _ => Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "descriptor not sent",
                )),
            }
        });
        match result {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            result => return result,
        }
    }
}

fn verify_membership(cgroup: &Path, pid: i32) -> anyhow::Result<()> {
    ensure!(pid > 1, "invalid peer PID");
    // Numeric PIDs are used only for observation; only the client may join itself.
    let read = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(cgroup.join("cgroup.procs"))?;
    ensure!(read.metadata()?.is_file(), "invalid membership file");
    let mut members = String::new();
    read.take(1024 * 1024 + 1).read_to_string(&mut members)?;
    ensure!(members.len() <= 1024 * 1024, "membership too large");
    ensure!(
        members
            .lines()
            .any(|line| line.parse::<i32>().ok() == Some(pid)),
        "membership not confirmed"
    );
    Ok(())
}

async fn handle<R, RF, Q, F>(
    mut stream: UnixStream,
    jobs: LocalJobs,
    hostname: &str,
    expected_uid: u32,
    resolve: R,
    query: Q,
) -> anyhow::Result<()>
where
    R: FnOnce(String) -> RF,
    RF: Future<Output = anyhow::Result<u32>>,
    Q: FnOnce(u32) -> F,
    F: Future<Output = anyhow::Result<JobInfo>>,
{
    let result = async {
        let peer = stream.peer_cred()?;
        validate_peer(peer.uid(), peer.pid(), expected_uid)?;
        let mut line = Vec::with_capacity(MAX_REQUEST);
        loop {
            let byte = stream.read_u8().await?;
            line.push(byte);
            if byte == b'\n' {
                break;
            }
            ensure!(line.len() < MAX_REQUEST, "request too long");
        }
        let (adopt, user) = parse_request(&line)?;
        let uid = resolve(user.to_owned()).await?;
        let snapshot = jobs.snapshot(user, uid).await?;
        let controller = query(snapshot.job_id).await?;
        jobs.finish(&snapshot, &controller, hostname, &mut stream, adopt)
            .await
    }
    .await;
    if result.is_err() {
        stream.write_all(b"DENY\n").await?;
    }
    Ok(())
}

fn safe_parent(path: &Path, uid: u32, trust_root: &Path, private: bool) -> anyhow::Result<()> {
    ensure!(
        path.is_absolute() && trust_root.is_absolute(),
        "absolute path required"
    );
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "non-normal path"
    );
    ensure!(
        path.starts_with(trust_root) && path != trust_root,
        "path outside trust root"
    );
    let parent = path.parent().context("missing parent")?;
    let mut dir = parent;
    loop {
        let meta = fs::symlink_metadata(dir)?;
        ensure!(
            meta.is_dir() && meta.uid() == uid && meta.mode() & 0o022 == 0,
            "unsafe parent directory"
        );
        if dir == parent && private {
            ensure!(
                meta.mode() & 0o7777 == 0o700,
                "socket parent must be mode 0700"
            );
        }
        if dir == trust_root {
            break;
        }
        dir = dir.parent().context("invalid trust root")?;
    }
    Ok(())
}

fn read_token(path: &Path, uid: u32, trust_root: &Path) -> anyhow::Result<MetadataValue<Ascii>> {
    safe_parent(path, uid, trust_root, false)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == uid
            && matches!(metadata.mode() & 0o7777, 0o400 | 0o600),
        "unsafe token file"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.dev() == metadata.dev()
            && opened.ino() == metadata.ino()
            && opened.uid() == uid
            && matches!(opened.mode() & 0o7777, 0o400 | 0o600),
        "token changed"
    );
    let mut token = String::new();
    file.take(16385).read_to_string(&mut token)?;
    ensure!(token.len() <= 16384, "token too large");
    let token = token.strip_suffix('\n').unwrap_or(&token);
    ensure!(
        !token.is_empty() && token.bytes().all(|c| c.is_ascii_graphic()),
        "invalid token"
    );
    let mut bearer: MetadataValue<Ascii> = format!("Bearer {token}").parse()?;
    bearer.set_sensitive(true);
    Ok(bearer)
}

struct OwnedSocket {
    path: PathBuf,
    dev: u64,
    ino: u64,
    uid: u32,
    trust_root: PathBuf,
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        if safe_parent(&self.path, self.uid, &self.trust_root, true).is_err() {
            return;
        }
        if let Ok(meta) = fs::symlink_metadata(&self.path) {
            if meta.file_type().is_socket()
                && meta.uid() == self.uid
                && meta.dev() == self.dev
                && meta.ino() == self.ino
                && meta.mode() & 0o7777 == 0o600
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

pub(crate) struct AdmissionListener {
    listener: UnixListener,
    _owned: OwnedSocket,
    bearer: MetadataValue<Ascii>,
}

impl AdmissionListener {
    pub(crate) fn bind(path: &Path, token: &Path) -> anyhow::Result<Self> {
        Self::bind_with_policy(path, token, 0, Path::new("/"))
    }

    fn bind_with_policy(
        path: &Path,
        token: &Path,
        uid: u32,
        trust_root: &Path,
    ) -> anyhow::Result<Self> {
        let bearer = read_token(token, uid, trust_root)?;
        safe_parent(path, uid, trust_root, true)?;
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => bail!("socket path already exists or cannot be inspected"),
        }
        let listener = UnixListener::bind(path)?;
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            meta.file_type().is_socket() && meta.uid() == uid,
            "invalid socket"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let owned = OwnedSocket {
            path: path.into(),
            dev: meta.dev(),
            ino: meta.ino(),
            uid,
            trust_root: trust_root.into(),
        };
        Ok(Self {
            listener,
            _owned: owned,
            bearer,
        })
    }

    pub(crate) async fn serve(
        self,
        jobs: LocalJobs,
        hostname: String,
        controller: String,
    ) -> anyhow::Result<()> {
        let mut handlers = JoinSet::new();
        let resolver = IdentityResolver::new();
        loop {
            tokio::select! {
                result = handlers.join_next(), if !handlers.is_empty() => {
                    result.context("missing handler")??;
                }
                accepted = self.listener.accept(), if handlers.len() < 32 => {
                    let (stream, _) = accepted?;
                    let (jobs, hostname, controller, bearer) = (jobs.clone(), hostname.clone(), controller.clone(), self.bearer.clone());
                    let resolver = resolver.clone();
                    handlers.spawn(async move {
                        let _ = tokio::time::timeout(Duration::from_secs(5), handle(
                            stream, jobs, &hostname, 0, move |user| async move { resolver.resolve(user).await },
                            move |job_id| async move {
                                let channel = spur_client::connect_channel(&controller).await?;
                                let mut client = spur_proto::controller_client(channel);
                                let mut request = tonic::Request::new(GetJobRequest { job_id });
                                request.metadata_mut().insert("authorization", bearer);
                                request.metadata_mut().insert(STRICT_HEADER, MetadataValue::from_static("1"));
                                let response = client.get_job(request).await?;
                                ensure!(response.metadata().get(STRICT_HEADER).is_some_and(|v| v == "1"), "controller did not acknowledge strict admission");
                                Ok(response.into_inner())
                            },
                        )).await;
                    });
                }
            }
        }
    }
}

pub(crate) fn validate_startup(
    config: &spur_core::config::SlurmConfig,
    uid: u32,
) -> anyhow::Result<()> {
    use spur_core::config::AuthMode;
    ensure!(uid == 0, "SSH admission requires root");
    ensure!(
        config.auth.mode == AuthMode::Required
            && !config
                .auth
                .jwt_key
                .as_deref()
                .unwrap_or_default()
                .is_empty(),
        "SSH admission requires authenticated agent RPCs"
    );
    ensure!(
        !config.auth.allow_root_jobs,
        "SSH admission forbids root jobs"
    );
    let c = &config.cgroup;
    ensure!(
        c.enabled
            && c.required
            && c.constrain_devices
            && c.constrain_cores
            && c.constrain_ram_space,
        "SSH admission requires device, CPU and memory cgroup enforcement"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_server::{new_running_jobs, TrackedJob};
    use std::io::Write;
    use std::os::fd::FromRawFd;

    async fn receive_first(stream: &UnixStream) -> (u8, Option<fs::File>) {
        loop {
            stream.readable().await.unwrap();
            let result = stream.try_io(Interest::READABLE, || {
                let mut byte = 0u8;
                let mut iov = libc::iovec {
                    iov_base: std::ptr::addr_of_mut!(byte).cast(),
                    iov_len: 1,
                };
                let mut control = [unsafe { std::mem::zeroed::<libc::cmsghdr>() }; 2];
                let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
                message.msg_iov = &mut iov;
                message.msg_iovlen = 1;
                message.msg_control = control.as_mut_ptr().cast();
                message.msg_controllen = std::mem::size_of_val(&control);
                let received = unsafe {
                    libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC)
                };
                if received == -1 {
                    return Err(io::Error::last_os_error());
                }
                assert_eq!(received, 1);
                assert_eq!(message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC), 0);
                let file = unsafe {
                    let header = libc::CMSG_FIRSTHDR(&message);
                    if header.is_null() {
                        None
                    } else {
                        assert_eq!((*header).cmsg_level, libc::SOL_SOCKET);
                        assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
                        assert_eq!(
                            (*header).cmsg_len,
                            libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as usize
                        );
                        let fd = libc::CMSG_DATA(header).cast::<libc::c_int>().read();
                        let file = fs::File::from_raw_fd(fd);
                        assert!(libc::CMSG_NXTHDR(&message, header).is_null());
                        let flags = libc::fcntl(fd, libc::F_GETFL);
                        assert!(flags >= 0);
                        assert_eq!(flags & libc::O_ACCMODE, libc::O_WRONLY);
                        assert_ne!(flags & libc::O_NONBLOCK, 0);
                        assert_ne!(flags & libc::O_NOFOLLOW, 0);
                        let descriptor_flags = libc::fcntl(fd, libc::F_GETFD);
                        assert!(descriptor_flags >= 0);
                        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
                        Some(file)
                    }
                };
                Ok((byte, file))
            });
            match result {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                result => return result.unwrap(),
            }
        }
    }

    async fn response_line(client: &mut UnixStream) -> String {
        let mut response = String::new();
        loop {
            let byte = client.read_u8().await.unwrap();
            response.push(char::from(byte));
            if byte == b'\n' {
                return response;
            }
            assert!(response.len() < 4096);
        }
    }

    fn info() -> JobInfo {
        JobInfo {
            job_id: 7,
            user: "alice".into(),
            uid: 1001,
            state: JobState::JobRunning as i32,
            exclusive: true,
            run_attempt: Some(0),
            nodelist: "node[01-02]".into(),
            start_time: Some(prost_types::Timestamp {
                seconds: 1,
                nanos: 0,
            }),
            ..Default::default()
        }
    }

    async fn fixture(eligible: bool) -> (tempfile::TempDir, LocalJobs) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("cgroup.procs"), "").unwrap();
        let jobs = LocalJobs {
            running: new_running_jobs(),
            lifecycle: JobLifecycle::default(),
        };
        jobs.running
            .lock()
            .await
            .insert(7, TrackedJob::ssh_fixture(dir.path().into(), eligible));
        (dir, jobs)
    }

    async fn exchange<Q, F>(jobs: LocalJobs, request: &[u8], uid: u32, query: Q) -> String
    where
        Q: FnOnce(u32) -> F,
        F: Future<Output = anyhow::Result<JobInfo>>,
    {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(request).await.unwrap();
        if !request.ends_with(b"\n") {
            client.shutdown().await.unwrap();
        }
        let serving = handle(
            server,
            jobs,
            "node01",
            uid,
            |name| async move {
                ensure!(name == "alice", "unexpected user");
                Ok(1001)
            },
            query,
        );
        let reading = async {
            let (first, descriptor) = receive_first(&client).await;
            if first == b'F' {
                assert_eq!(request, b"SPURSSH1 ADOPT alice\n");
                let mut file = descriptor.expect("ADOPT must pass a descriptor");
                file.write_all(b"0\n").unwrap();
                drop(file);
                client.write_all(b"JOINED\n").await.unwrap();
                response_line(&mut client).await
            } else {
                assert!(
                    descriptor.is_none(),
                    "CHECK and DENY must not pass descriptors"
                );
                format!("{}{}", char::from(first), response_line(&mut client).await)
            }
        };
        let (result, response) = tokio::join!(serving, reading);
        result.unwrap();
        response
    }

    fn test_uid() -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    #[test]
    fn parser_is_strict_and_bounded() {
        assert_eq!(
            parse_request(b"SPURSSH1 CHECK alice\n").unwrap(),
            (false, "alice")
        );
        assert_eq!(
            parse_request(b"SPURSSH1 ADOPT a_1.-\n").unwrap(),
            (true, "a_1.-")
        );
        for line in [
            b"SPURSSH1 CHECK root\n".as_slice(),
            b"SPURSSH1 CHECK alice\r\n",
            b"SPURSSH1 CHECK alice extra\n",
            b"SPURSSH1  CHECK alice\n",
            b"SPURSSH1 CHECK a/b\n",
            b"SPURSSH1 CHECK alice",
            b"SPURSSH1 ADOPT \n",
            b"SPURSSH1 CHECK a\xff\n",
        ] {
            assert!(parse_request(line).is_err(), "{line:?}");
        }
        assert!(parse_request(&vec![b'x'; 257]).is_err());
        assert!(parse_request(format!("SPURSSH1 CHECK {}\n", "a".repeat(129)).as_bytes()).is_err());
        for pid in [None, Some(-1), Some(0), Some(1)] {
            assert!(validate_peer(0, pid, 0).is_err());
        }
    }

    #[tokio::test]
    async fn real_handler_refuses_peer_and_malformed_requests() {
        let (_dir, jobs) = fixture(true).await;
        let unexpected = |_| async {
            panic!("controller must not be queried");
            #[allow(unreachable_code)]
            Ok(info())
        };
        assert_eq!(
            exchange(
                jobs.clone(),
                b"SPURSSH1 CHECK alice\n",
                test_uid().wrapping_add(1),
                unexpected
            )
            .await,
            "DENY\n"
        );
        assert_eq!(
            exchange(
                jobs.clone(),
                b"SPURSSH1 CHECK root\n",
                test_uid(),
                unexpected
            )
            .await,
            "DENY\n"
        );
        assert_eq!(
            exchange(jobs, &vec![b'x'; 257], test_uid(), unexpected).await,
            "DENY\n"
        );
    }

    #[tokio::test]
    async fn check_never_passes_fd_and_only_client_writes_zero() {
        let (dir, jobs) = fixture(true).await;
        assert_eq!(
            exchange(
                jobs.clone(),
                b"SPURSSH1 CHECK alice\n",
                test_uid(),
                |_| async { Ok(info()) }
            )
            .await,
            "OK 7 0,2\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
            ""
        );
        let members = format!("0\n{}\n", std::process::id());
        fs::write(dir.path().join("cgroup.procs"), &members).unwrap();
        assert_eq!(
            exchange(jobs, b"SPURSSH1 ADOPT alice\n", test_uid(), |_| async {
                Ok(info())
            })
            .await,
            "OK 7 0,2\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
            members
        );
    }

    #[tokio::test]
    async fn receipt_wrong_truncated_or_disconnected_fails_closed() {
        for receipt in [b"WRONG!\n".as_slice(), b"JOINED", b""] {
            let (dir, jobs) = fixture(true).await;
            let members = format!("0\n{}\n", std::process::id());
            fs::write(dir.path().join("cgroup.procs"), &members).unwrap();
            let (mut client, server) = UnixStream::pair().unwrap();
            client.write_all(b"SPURSSH1 ADOPT alice\n").await.unwrap();
            let serving = handle(
                server,
                jobs.clone(),
                "node01",
                test_uid(),
                |_| async { Ok(1001) },
                |_| async { Ok(info()) },
            );
            let reading = async {
                let (marker, file) = receive_first(&client).await;
                assert_eq!(marker, b'F');
                drop(file.unwrap());
                assert!(jobs.running.try_lock().is_ok());
                let lifecycle = jobs.lifecycle.acquire(7);
                tokio::pin!(lifecycle);
                assert!(
                    std::future::poll_fn(|cx| std::task::Poll::Ready(
                        lifecycle.as_mut().poll(cx).is_pending()
                    ))
                    .await
                );
                client.write_all(receipt).await.unwrap();
                client.shutdown().await.unwrap();
                assert_eq!(response_line(&mut client).await, "DENY\n");
            };
            let (result, ()) = tokio::join!(serving, reading);
            result.unwrap();
            assert!(jobs.running.try_lock().is_ok());
            drop(jobs.lifecycle.acquire(7).await);
            assert_eq!(
                fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
                members
            );
        }
    }

    #[tokio::test]
    async fn client_zero_without_peer_membership_is_denied() {
        let (dir, jobs) = fixture(true).await;
        assert_eq!(
            exchange(jobs, b"SPURSSH1 ADOPT alice\n", test_uid(), |_| async {
                Ok(info())
            })
            .await,
            "DENY\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
            "0\n"
        );
    }

    #[tokio::test]
    async fn disconnect_after_fd_releases_locks_without_writes() {
        let (dir, jobs) = fixture(true).await;
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"SPURSSH1 ADOPT alice\n").await.unwrap();
        let serving = handle(
            server,
            jobs.clone(),
            "node01",
            test_uid(),
            |_| async { Ok(1001) },
            |_| async { Ok(info()) },
        );
        let disconnect = async move {
            let (marker, file) = receive_first(&client).await;
            assert_eq!(marker, b'F');
            drop(file.unwrap());
            drop(client);
        };
        let (result, ()) = tokio::join!(serving, disconnect);
        assert!(result.is_err());
        assert!(jobs.running.try_lock().is_ok());
        drop(jobs.lifecycle.acquire(7).await);
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
            ""
        );
    }

    #[tokio::test(start_paused = true)]
    async fn outer_deadline_cancels_handshake_and_releases_both_locks() {
        let (dir, jobs) = fixture(true).await;
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"SPURSSH1 ADOPT alice\n").await.unwrap();
        let serving = tokio::time::timeout(
            Duration::from_secs(5),
            handle(
                server,
                jobs.clone(),
                "node01",
                test_uid(),
                |_| async { Ok(1001) },
                |_| async { Ok(info()) },
            ),
        );
        let stalled = async {
            let (marker, file) = receive_first(&client).await;
            assert_eq!(marker, b'F');
            drop(file.unwrap());
            assert!(jobs.running.try_lock().is_ok());
            tokio::time::advance(Duration::from_secs(5)).await;
        };
        let (result, ()) = tokio::join!(serving, stalled);
        assert!(result.is_err());
        assert!(jobs.running.try_lock().is_ok());
        drop(jobs.lifecycle.acquire(7).await);
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn allocation_removed_during_handoff_cannot_receive_final_ok() {
        let (dir, jobs) = fixture(true).await;
        fs::write(
            dir.path().join("cgroup.procs"),
            format!("0\n{}\n", std::process::id()),
        )
        .unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"SPURSSH1 ADOPT alice\n").await.unwrap();
        let serving = handle(
            server,
            jobs.clone(),
            "node01",
            test_uid(),
            |_| async { Ok(1001) },
            |_| async { Ok(info()) },
        );
        let removed = async {
            let (marker, file) = receive_first(&client).await;
            assert_eq!(marker, b'F');
            drop(file.unwrap());
            jobs.running.lock().await.remove(&7);
            client.write_all(b"JOINED\n").await.unwrap();
            assert_eq!(response_line(&mut client).await, "DENY\n");
        };
        let (result, ()) = tokio::join!(serving, removed);
        result.unwrap();
    }

    #[tokio::test]
    async fn membership_is_rechecked_after_receipt() {
        let (dir, jobs) = fixture(true).await;
        let members = format!("0\n{}\n", std::process::id());
        fs::write(dir.path().join("cgroup.procs"), &members).unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"SPURSSH1 ADOPT alice\n").await.unwrap();
        let serving = handle(
            server,
            jobs,
            "node01",
            test_uid(),
            |_| async { Ok(1001) },
            |_| async { Ok(info()) },
        );
        let reading = async {
            let (marker, file) = receive_first(&client).await;
            assert_eq!(marker, b'F');
            drop(file.unwrap());
            // Removing membership during the exchange must prevent final success.
            fs::write(dir.path().join("cgroup.procs"), "0\n").unwrap();
            client.write_all(b"JOINED\n").await.unwrap();
            assert_eq!(response_line(&mut client).await, "DENY\n");
        };
        let (result, ()) = tokio::join!(serving, reading);
        result.unwrap();
    }

    #[tokio::test]
    async fn inactive_missing_and_ambiguous_allocations_deny() {
        let (dir, jobs) = fixture(false).await;
        let unexpected = |_| async {
            panic!("controller must not be queried");
            #[allow(unreachable_code)]
            Ok(info())
        };
        assert_eq!(
            exchange(
                jobs.clone(),
                b"SPURSSH1 CHECK alice\n",
                test_uid(),
                unexpected
            )
            .await,
            "DENY\n"
        );
        {
            let mut running = jobs.running.lock().await;
            running.insert(7, TrackedJob::ssh_fixture(dir.path().into(), true));
            running.insert(8, TrackedJob::ssh_fixture(dir.path().into(), true));
        }
        assert_eq!(
            exchange(
                jobs.clone(),
                b"SPURSSH1 ADOPT alice\n",
                test_uid(),
                unexpected
            )
            .await,
            "DENY\n"
        );
        jobs.running.lock().await.clear();
        assert_eq!(
            exchange(jobs, b"SPURSSH1 CHECK alice\n", test_uid(), unexpected).await,
            "DENY\n"
        );
    }

    #[tokio::test]
    async fn replacement_with_same_attempt_and_revocation_deny() {
        for eligible in [true, false] {
            let (dir, jobs) = fixture(true).await;
            let mutate = jobs.clone();
            let path = dir.path().to_owned();
            assert_eq!(
                exchange(
                    jobs,
                    b"SPURSSH1 ADOPT alice\n",
                    test_uid(),
                    |_| async move {
                        mutate
                            .running
                            .lock()
                            .await
                            .insert(7, TrackedJob::ssh_fixture(path, eligible));
                        Ok(info())
                    }
                )
                .await,
                "DENY\n"
            );
            assert_eq!(
                fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
                ""
            );
        }
    }

    #[tokio::test]
    async fn controller_mismatch_and_errors_fail_closed() {
        let (dir, jobs) = fixture(true).await;
        let mut bad = Vec::new();
        let mut v = info();
        v.state = JobState::JobSuspended as i32;
        bad.push(v);
        let mut v = info();
        v.run_attempt = Some(1);
        bad.push(v);
        let mut v = info();
        v.run_attempt = None;
        bad.push(v);
        let mut v = info();
        v.uid = 0;
        bad.push(v);
        let mut v = info();
        v.user = "bob".into();
        bad.push(v);
        let mut v = info();
        v.job_id = 8;
        bad.push(v);
        let mut v = info();
        v.exclusive = false;
        bad.push(v);
        let mut v = info();
        v.nodelist = "elsewhere".into();
        bad.push(v);
        let mut v = info();
        v.start_time = None;
        bad.push(v);
        let mut v = info();
        v.end_time = v.start_time;
        bad.push(v);
        let mut v = info();
        v.time_limit = Some(prost_types::Duration {
            seconds: 1,
            nanos: 0,
        });
        bad.push(v);
        let mut v = info();
        v.time_limit = Some(prost_types::Duration {
            seconds: -1,
            nanos: 0,
        });
        bad.push(v);
        let mut v = info();
        v.time_limit = Some(prost_types::Duration {
            seconds: 0,
            nanos: 0,
        });
        bad.push(v);
        for v in bad {
            assert_eq!(
                exchange(
                    jobs.clone(),
                    b"SPURSSH1 ADOPT alice\n",
                    test_uid(),
                    |_| async { Ok(v) }
                )
                .await,
                "DENY\n"
            );
        }
        assert_eq!(
            exchange(jobs, b"SPURSSH1 ADOPT alice\n", test_uid(), |_| async {
                bail!("controller unavailable")
            })
            .await,
            "DENY\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn same_generation_controller_revocation_denies_finish() {
        let (dir, jobs) = fixture(true).await;
        let snapshot = jobs.snapshot("alice", 1001).await.unwrap();
        for state in [JobState::JobSuspended, JobState::JobCancelled] {
            let mut controller = info();
            controller.state = state as i32;
            for adopt in [false, true] {
                let (_client, mut server) = UnixStream::pair().unwrap();
                assert!(jobs
                    .finish(&snapshot, &controller, "node01", &mut server, adopt)
                    .await
                    .is_err());
                let current = jobs.snapshot("alice", 1001).await.unwrap();
                assert!(Arc::ptr_eq(&snapshot.generation, &current.generation));
            }
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
            ""
        );
        let (mut client, mut server) = UnixStream::pair().unwrap();
        jobs.finish(&snapshot, &info(), "node01", &mut server, false)
            .await
            .unwrap();
        assert_eq!(response_line(&mut client).await, "OK 7 0,2\n");
    }

    #[test]
    fn production_startup_requires_every_enforcement_flag() {
        use spur_core::config::{AuthMode, SlurmConfig};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spur.conf");
        fs::write(
            &path,
            r#"
cluster_name = "ssh-admission-test"
[auth]
plugin = "jwt"
mode = "required"
jwt_key = "test-only-not-a-production-key"
allow_root_jobs = false
[cgroup]
enabled = true
required = true
constrain_devices = true
constrain_cores = true
constrain_ram_space = true
"#,
        )
        .unwrap();
        let valid = SlurmConfig::load_from_file(&path).unwrap();
        validate_startup(&valid, 0).unwrap();
        assert!(validate_startup(&valid, 1001).is_err());
        for mode in [AuthMode::Disabled, AuthMode::Permissive] {
            let mut config = valid.clone();
            config.auth.mode = mode;
            assert!(validate_startup(&config, 0).is_err(), "{mode:?}");
        }
        for key in [None, Some(String::new())] {
            let mut config = valid.clone();
            config.auth.jwt_key = key;
            assert!(validate_startup(&config, 0).is_err());
        }
        let mut config = valid.clone();
        config.auth.allow_root_jobs = true;
        assert!(validate_startup(&config, 0).is_err());
        for flag in [
            "enabled",
            "required",
            "constrain_devices",
            "constrain_cores",
            "constrain_ram_space",
        ] {
            let mut config = valid.clone();
            match flag {
                "enabled" => config.cgroup.enabled = false,
                "required" => config.cgroup.required = false,
                "constrain_devices" => config.cgroup.constrain_devices = false,
                "constrain_cores" => config.cgroup.constrain_cores = false,
                "constrain_ram_space" => config.cgroup.constrain_ram_space = false,
                _ => unreachable!(),
            }
            assert!(validate_startup(&config, 0).is_err(), "missing {flag}");
        }
    }

    #[tokio::test]
    async fn malformed_controller_timestamps_deny_before_membership_write() {
        let (dir, jobs) = fixture(true).await;
        for (seconds, nanos) in [(-1, 0), (1, -1), (1, 1_000_000_000)] {
            for malformed_start in [true, false] {
                let mut controller = info();
                if malformed_start {
                    controller.start_time = Some(prost_types::Timestamp { seconds, nanos });
                } else {
                    controller.time_limit = Some(prost_types::Duration { seconds, nanos });
                }
                assert_eq!(
                    exchange(
                        jobs.clone(),
                        b"SPURSSH1 ADOPT alice\n",
                        test_uid(),
                        |_| async { Ok(controller) }
                    )
                    .await,
                    "DENY\n",
                    "start={malformed_start}, seconds={seconds}, nanos={nanos}"
                );
            }
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn controller_deadline_is_exclusive_at_nanosecond_precision() {
        let (_dir, jobs) = fixture(true).await;
        let snapshot = jobs.snapshot("alice", 1001).await.unwrap();
        let mut controller = info();
        controller.start_time = Some(prost_types::Timestamp {
            seconds: 10,
            nanos: 999_999_999,
        });
        controller.time_limit = Some(prost_types::Duration {
            seconds: 0,
            nanos: 2,
        });
        let start = UNIX_EPOCH + Duration::new(10, 999_999_999);
        let deadline = UNIX_EPOCH + Duration::new(11, 1);
        assert!(validate_controller(
            &controller,
            &snapshot,
            "node01",
            start - Duration::from_nanos(1)
        )
        .is_err());
        for now in [start, deadline - Duration::from_nanos(1)] {
            validate_controller(&controller, &snapshot, "node01", now).unwrap();
        }
        for now in [deadline, deadline + Duration::from_nanos(1)] {
            assert!(validate_controller(&controller, &snapshot, "node01", now).is_err());
        }
        controller.time_limit = None;
        validate_controller(&controller, &snapshot, "node01", deadline).unwrap();
    }

    #[test]
    fn membership_never_creates_truncates_or_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cgroup.procs");
        assert!(open_membership(dir.path()).is_err());
        assert!(verify_membership(dir.path(), 123).is_err());
        assert!(!path.exists());
        fs::write(&path, "999\n456\n").unwrap();
        let file = open_membership(dir.path()).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        drop(file);
        verify_membership(dir.path(), 456).unwrap();
        assert!(verify_membership(dir.path(), 123).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "999\n456\n");
        fs::remove_file(&path).unwrap();
        let target = dir.path().join("target");
        fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(open_membership(dir.path()).is_err());
        assert!(verify_membership(dir.path(), 123).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
        fs::remove_file(&path).unwrap();
        let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(open_membership(dir.path()).is_err());
        assert!(verify_membership(dir.path(), 123).is_err());
    }

    #[tokio::test]
    async fn unsafe_paths_and_existing_sockets_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let token = dir.path().join("token");
        fs::write(&token, "explicit-admin-credential\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        let socket = dir.path().join("admission.sock");
        let bind = || AdmissionListener::bind_with_policy(&socket, &token, test_uid(), dir.path());
        let listener = bind().unwrap();
        assert_eq!(fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
        assert!(bind().is_err());
        drop(listener);
        assert!(!socket.exists());
        for mode in [0o644, 0o660, 0o700] {
            fs::set_permissions(&token, fs::Permissions::from_mode(mode)).unwrap();
            assert!(bind().is_err());
        }
        fs::set_permissions(&token, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(read_token(&token, test_uid().wrapping_add(1), dir.path()).is_err());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(bind().is_err());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let actual = dir.path().join("actual");
        fs::rename(&token, &actual).unwrap();
        std::os::unix::fs::symlink(&actual, &token).unwrap();
        assert!(bind().is_err());
        assert!(safe_parent(&dir.path().join("../escape"), test_uid(), dir.path(), true).is_err());
    }

    #[tokio::test]
    async fn cleanup_preserves_replacement_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let token = dir.path().join("token");
        fs::write(&token, "credential").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        let socket = dir.path().join("socket");
        let listener =
            AdmissionListener::bind_with_policy(&socket, &token, test_uid(), dir.path()).unwrap();
        fs::remove_file(&socket).unwrap();
        fs::write(&socket, "replacement").unwrap();
        drop(listener);
        assert_eq!(fs::read_to_string(socket).unwrap(), "replacement");
    }
}
