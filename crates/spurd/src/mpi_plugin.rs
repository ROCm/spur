// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runtime-loaded PMIx plugin host. A supervised launch hosts its server in
//! `spurstepd`; the agent keeps one only for the launches it runs itself.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use libloading::{Library, Symbol};
use spur_core::config::MpiConfig;
use spur_core::mpi::{self, PmixLaunchPlan};
use spur_core::step::StepId;
use tracing::{info, warn};

/// Keys required in per-rank PMIx setup_fork env.
const PMIX_ENV_KEYS: &[&str] = &[
    "PMIX_SERVER_URI",
    "PMIX_SERVER_URI4",
    "PMIX_NAMESPACE",
    "PMIX_RANK",
    "PMIX_SIZE",
    "PMIX_JOB_SIZE",
    "PMIX_SERVER_TMPDIR",
];

#[repr(C)]
#[derive(Copy, Clone)]
struct SpurMpiProc {
    rank: c_uint,
    local_rank: c_uint,
}

#[repr(C)]
struct SpurMpiLaunchPlan {
    job_id: c_uint,
    step_id: c_uint,
    namespace: [c_char; 256],
    universe_size: c_uint,
    task_offset: c_uint,
    num_local_procs: c_uint,
    local_procs: [SpurMpiProc; 256],
    tmpdir: [c_char; 512],
    job_uid: c_uint,
    job_gid: c_uint,
    num_nodes: c_uint,
    node_index: c_uint,
    num_peer_hosts: c_uint,
    peer_hosts: [[c_char; 256]; 64],
    modex_connect_timeout_sec: c_uint,
    modex_fence_timeout_sec: c_uint,
    modex_verify_timeout_sec: c_uint,
}

type VersionFn = unsafe extern "C" fn() -> c_int;
type RuntimeVersionFn = unsafe extern "C" fn(*mut c_char, usize) -> c_int;
type ServerStartFn = unsafe extern "C" fn(*const SpurMpiLaunchPlan, *mut c_char, usize) -> c_int;
type ServerStopFn = unsafe extern "C" fn(*const c_char, *mut c_char, usize) -> c_int;
type VerifyPeersFn = unsafe extern "C" fn(*const SpurMpiLaunchPlan, *mut c_char, usize) -> c_int;
type SetupForkEnvFn =
    unsafe extern "C" fn(*const SpurMpiLaunchPlan, c_uint, *mut *mut *mut c_char) -> c_int;
type SetupForkEnvFreeFn = unsafe extern "C" fn(*mut *mut c_char);

struct PluginApi {
    _library: Library,
    server_start: ServerStartFn,
    server_stop: ServerStopFn,
    verify_peers: VerifyPeersFn,
    setup_fork_env: SetupForkEnvFn,
    setup_fork_env_free: SetupForkEnvFreeFn,
}

/// A PMIx rendezvous is per-step: one job can run several steps on a node at
/// once, and each gets exactly one server, owned by whoever started it.
pub(crate) type NamespaceKey = (u32, StepId);

pub struct MpiPluginHost {
    config: MpiConfig,
    plugin: Mutex<Option<PluginApi>>,
    pub(crate) active_namespaces: Mutex<HashMap<NamespaceKey, String>>,
}

/// Rolls back a PMIx namespace reference when launch fails before the job is committed.
pub struct PmixLaunchGuard {
    host: Arc<MpiPluginHost>,
    key: NamespaceKey,
    rollback: bool,
}

impl PmixLaunchGuard {
    pub fn start(host: Arc<MpiPluginHost>, plan: &PmixLaunchPlan) -> Result<Self, String> {
        host.start_pmix_server(plan)?;
        Ok(Self {
            host,
            key: (plan.job_id, plan.step_id),
            rollback: true,
        })
    }

    pub fn disarm(&mut self) {
        self.rollback = false;
    }
}

impl Drop for PmixLaunchGuard {
    fn drop(&mut self) {
        if !self.rollback {
            return;
        }
        let (job_id, step_id) = self.key;
        if let Err(err) = self.host.release_hosted_pmix_server(job_id, step_id) {
            warn!(job_id, step_id, error = %err, "PMIx rollback release failed");
        }
    }
}

impl MpiPluginHost {
    pub fn new(config: MpiConfig) -> Self {
        Self {
            config,
            plugin: Mutex::new(None),
            active_namespaces: Mutex::new(HashMap::new()),
        }
    }

    fn apply_modex_timeouts(&self, plan: &mut PmixLaunchPlan) {
        if plan.modex_connect_timeout_secs == 0 {
            plan.modex_connect_timeout_secs = self.config.modex_connect_timeout_secs;
        }
        if plan.modex_fence_timeout_secs == 0 {
            plan.modex_fence_timeout_secs = self.config.modex_fence_timeout_secs;
        }
        if plan.modex_verify_timeout_secs == 0 {
            plan.modex_verify_timeout_secs = self.config.modex_verify_timeout_secs;
        }
    }

    pub fn plugin_path(&self) -> PathBuf {
        self.config.resolve_pmix_plugin_path()
    }

    /// The resolved [mpi] settings, handed to a supervisor that hosts its own
    /// server and has no config file of its own to read them from.
    pub fn config(&self) -> &MpiConfig {
        &self.config
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn has_active_pmix(&self, job_id: u32, step_id: StepId) -> bool {
        match self.active_namespaces.lock() {
            Ok(guard) => guard.contains_key(&(job_id, step_id)),
            Err(_) => true,
        }
    }

    fn load_plugin(&self) -> Result<(), String> {
        let mut guard = self
            .plugin
            .lock()
            .map_err(|_| "plugin lock poisoned".to_string())?;
        if guard.is_some() {
            return Ok(());
        }

        let path = self.plugin_path();
        if !path.is_file() {
            return Err(format!(
                "MPI plugin not found at {} (install spur_mpi_pmix.so or set [mpi].plugin_dir)",
                path.display()
            ));
        }

        let library = unsafe { Library::new(&path) }.map_err(|e| {
            format!(
                "failed to load MPI plugin {}: {e} (is libpmix installed on this node?)",
                path.display()
            )
        })?;

        let version: Symbol<VersionFn> = unsafe { library.get(b"spur_mpi_pmix_version") }
            .map_err(|e| format!("MPI plugin missing spur_mpi_pmix_version: {e}"))?;
        let runtime_version: Symbol<RuntimeVersionFn> =
            unsafe { library.get(b"spur_mpi_pmix_runtime_version") }
                .map_err(|e| format!("MPI plugin missing spur_mpi_pmix_runtime_version: {e}"))?;
        let server_start: Symbol<ServerStartFn> =
            unsafe { library.get(b"spur_mpi_pmix_server_start") }
                .map_err(|e| format!("MPI plugin missing spur_mpi_pmix_server_start: {e}"))?;
        let server_stop: Symbol<ServerStopFn> =
            unsafe { library.get(b"spur_mpi_pmix_server_stop") }
                .map_err(|e| format!("MPI plugin missing spur_mpi_pmix_server_stop: {e}"))?;
        let verify_peers: Symbol<VerifyPeersFn> =
            unsafe { library.get(b"spur_mpi_pmix_verify_peers") }
                .map_err(|e| format!("MPI plugin missing spur_mpi_pmix_verify_peers: {e}"))?;
        let setup_fork_env: Symbol<SetupForkEnvFn> =
            unsafe { library.get(b"spur_mpi_pmix_setup_fork_env") }
                .map_err(|e| format!("MPI plugin missing spur_mpi_pmix_setup_fork_env: {e}"))?;
        let setup_fork_env_free: Symbol<SetupForkEnvFreeFn> = unsafe {
            library.get(b"spur_mpi_pmix_setup_fork_env_free")
        }
        .map_err(|e| format!("MPI plugin missing spur_mpi_pmix_setup_fork_env_free: {e}"))?;

        let api_version = unsafe { version() };
        if api_version != 4 {
            return Err(format!(
                "unsupported MPI plugin API version {api_version} (expected 4)"
            ));
        }

        let mut runtime_buf = vec![0i8; 256];
        let runtime_rc = unsafe { runtime_version(runtime_buf.as_mut_ptr(), runtime_buf.len()) };
        if runtime_rc == 0 {
            let runtime = c_str_to_string(&runtime_buf);
            info!(plugin = %path.display(), pmix_version = %runtime, "loaded MPI plugin");
            if !self.config.pmix_min_version.is_empty()
                && !mpi::version_at_least(&runtime, &self.config.pmix_min_version)
            {
                return Err(format!(
                    "PMIx runtime {runtime} is older than required {} (see [mpi].pmix_min_version)",
                    self.config.pmix_min_version
                ));
            }
        } else {
            warn!(
                plugin = %path.display(),
                "MPI plugin has no linked PMIx runtime (stub build?)"
            );
        }

        let server_start_fn = *server_start;
        let server_stop_fn = *server_stop;
        let verify_peers_fn = *verify_peers;
        let setup_fork_env_fn = *setup_fork_env;
        let setup_fork_env_free_fn = *setup_fork_env_free;

        *guard = Some(PluginApi {
            _library: library,
            server_start: server_start_fn,
            server_stop: server_stop_fn,
            verify_peers: verify_peers_fn,
            setup_fork_env: setup_fork_env_fn,
            setup_fork_env_free: setup_fork_env_free_fn,
        });
        Ok(())
    }

    fn call_server_start(&self, plan: &PmixLaunchPlan) -> Result<(), String> {
        let c_plan = plan_to_c(plan)?;
        let mut errbuf = vec![0i8; 512];
        let rc = {
            let guard = self
                .plugin
                .lock()
                .map_err(|_| "plugin lock poisoned".to_string())?;
            let api = guard
                .as_ref()
                .ok_or_else(|| "MPI plugin not loaded".to_string())?;
            unsafe { (api.server_start)(&c_plan, errbuf.as_mut_ptr(), errbuf.len()) }
        };
        if rc != 0 {
            return Err(c_str_to_string(&errbuf));
        }
        Ok(())
    }

    fn call_verify_peers(&self, plan: &PmixLaunchPlan) -> Result<(), String> {
        let c_plan = plan_to_c(plan)?;
        let mut errbuf = vec![0i8; 512];
        let rc = {
            let guard = self
                .plugin
                .lock()
                .map_err(|_| "plugin lock poisoned".to_string())?;
            let api = guard
                .as_ref()
                .ok_or_else(|| "MPI plugin not loaded".to_string())?;
            unsafe { (api.verify_peers)(&c_plan, errbuf.as_mut_ptr(), errbuf.len()) }
        };
        if rc != 0 {
            return Err(c_str_to_string(&errbuf));
        }
        Ok(())
    }

    fn call_server_stop(&self, namespace: &str) -> Result<(), String> {
        let c_namespace =
            CString::new(namespace).map_err(|_| "invalid PMIx namespace".to_string())?;
        let guard = self
            .plugin
            .lock()
            .map_err(|_| "plugin lock poisoned".to_string())?;
        let Some(api) = guard.as_ref() else {
            warn!(
                namespace,
                "PMIx plugin not loaded during stop; skipping C server_stop"
            );
            return Ok(());
        };
        let mut errbuf = vec![0i8; 256];
        let rc =
            unsafe { (api.server_stop)(c_namespace.as_ptr(), errbuf.as_mut_ptr(), errbuf.len()) };
        if rc != 0 {
            let err = c_str_to_string(&errbuf);
            warn!(namespace, error = %err, "PMIx server stop failed");
            return Err(err);
        }
        info!(namespace, "PMIx server stopped");
        Ok(())
    }

    /// Start this node's PMIx server for a step's namespace. Exactly one server
    /// exists per (job, step), owned until the caller releases or stops it.
    pub fn start_pmix_server(&self, plan: &PmixLaunchPlan) -> Result<(), String> {
        let mut plan = plan.clone();
        self.apply_modex_timeouts(&mut plan);
        mpi::validate_pmix_plan(&plan)?;

        let key = (plan.job_id, plan.step_id);
        if self
            .active_namespaces
            .lock()
            .map_err(|_| "namespace lock poisoned".to_string())?
            .contains_key(&key)
        {
            return Err(format!(
                "a PMIx server is already running for job {} step {}",
                plan.job_id, plan.step_id
            ));
        }

        if let Err(err) = self
            .load_plugin()
            .and_then(|()| self.call_server_start(&plan))
        {
            warn!(
                job_id = plan.job_id,
                step_id = plan.step_id,
                namespace = %plan.namespace,
                error = %err,
                "PMIx server start failed"
            );
            return Err(err);
        }

        // The cross-node modex rendezvous is checked by whoever hosts the
        // server, not by a separate pre-flight on the agent.
        if plan.num_nodes > 1 {
            if let Err(err) = self.call_verify_peers(&plan) {
                if let Err(stop_err) = self.call_server_stop(&plan.namespace) {
                    warn!(
                        job_id = plan.job_id,
                        namespace = %plan.namespace,
                        error = %stop_err,
                        "PMIx server stop failed while rolling back a failed peer verify"
                    );
                }
                return Err(err);
            }
        }

        self.active_namespaces
            .lock()
            .map_err(|_| "namespace lock poisoned".to_string())?
            .insert(key, plan.namespace.clone());
        info!(
            job_id = plan.job_id,
            step_id = plan.step_id,
            namespace = %plan.namespace,
            universe_size = plan.universe_size,
            local_procs = plan.local_procs.len(),
            "PMIx server started"
        );
        Ok(())
    }

    /// Pre-flight for a `--mpi=pmix` dispatch: the plan is well formed and this
    /// node can load a PMIx runtime new enough to serve it. Starts nothing.
    pub fn validate_pmix_dispatch(&self, plan: &PmixLaunchPlan) -> Result<(), String> {
        let mut plan = plan.clone();
        self.apply_modex_timeouts(&mut plan);
        mpi::validate_pmix_plan(&plan)?;
        self.load_plugin()
    }

    /// Stop the server registered for one step. An unknown key is an error: a
    /// silent `Ok` there hides a teardown that never happened.
    pub fn release_pmix_server(&self, job_id: u32, step_id: StepId) -> Result<(), String> {
        match self.release_hosted_pmix_server(job_id, step_id)? {
            true => Ok(()),
            false => Err(format!(
                "no PMIx server registered for job {job_id} step {step_id}"
            )),
        }
    }

    /// Releases only if this process still hosts it, reporting whether it did.
    /// A rollback races teardown, and finding it already stopped is success.
    pub fn release_hosted_pmix_server(&self, job_id: u32, step_id: StepId) -> Result<bool, String> {
        let key = (job_id, step_id);
        let Some(namespace) = self
            .active_namespaces
            .lock()
            .map_err(|_| "namespace lock poisoned".to_string())?
            .remove(&key)
        else {
            return Ok(false);
        };
        self.call_server_stop(&namespace)
            .map(|()| true)
            .inspect_err(|err| {
                warn!(
                    job_id,
                    step_id,
                    namespace = %namespace,
                    error = %err,
                    "PMIx server stop failed — the namespace entry was evicted anyway"
                );
            })
    }

    /// Stop every PMIx namespace this process hosts for a job. Cancel and
    /// reclaim teardown name only the job, so they land here. In an agent this
    /// normally finds nothing — servers live in supervisors — so its `Ok` is not
    /// evidence that a teardown happened.
    pub fn stop_pmix_job(&self, job_id: u32) -> Result<(), String> {
        let hosted: Vec<NamespaceKey> = self
            .active_namespaces
            .lock()
            .map_err(|_| "namespace lock poisoned".to_string())?
            .keys()
            .filter(|(id, _)| *id == job_id)
            .copied()
            .collect();
        let mut failure = None;
        for (job_id, step_id) in hosted {
            if let Err(err) = self.release_pmix_server(job_id, step_id) {
                failure = Some(err);
            }
        }
        match failure {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Bulk `PMIx_server_setup_fork` env for one rank.
    pub fn pmix_setup_fork_env(
        &self,
        plan: &PmixLaunchPlan,
        rank: u32,
    ) -> Result<HashMap<String, String>, String> {
        mpi::validate_pmix_plan(plan)?;
        self.load_plugin()?;
        let c_plan = plan_to_c(plan)?;
        let mut env_ptr: *mut *mut c_char = std::ptr::null_mut();
        let rc = {
            let guard = self
                .plugin
                .lock()
                .map_err(|_| "plugin lock poisoned".to_string())?;
            let api = guard
                .as_ref()
                .ok_or_else(|| "MPI plugin not loaded".to_string())?;
            unsafe { (api.setup_fork_env)(&c_plan, rank, &mut env_ptr) }
        };
        if rc != 0 {
            return Err(format!(
                "PMIx_server_setup_fork failed for job {} rank {rank}",
                plan.job_id
            ));
        }
        let mut out = parse_setup_fork_env(env_ptr);
        {
            let guard = self
                .plugin
                .lock()
                .map_err(|_| "plugin lock poisoned".to_string())?;
            let api = guard
                .as_ref()
                .ok_or_else(|| "MPI plugin not loaded".to_string())?;
            unsafe { (api.setup_fork_env_free)(env_ptr) };
        }
        normalize_pmix_fork_env(&mut out, plan, rank);
        Ok(out)
    }
}

fn normalize_pmix_fork_env(env: &mut HashMap<String, String>, plan: &PmixLaunchPlan, rank: u32) {
    apply_pmix_uri_aliases(env);

    let size = plan.universe_size.to_string();
    env.insert("PMIX_SIZE".into(), size.clone());
    env.insert("PMIX_JOB_SIZE".into(), size.clone());
    env.insert("PMIX_APP_SIZE".into(), size);

    env.entry("PMIX_NAMESPACE".into())
        .or_insert_with(|| plan.namespace.clone());
    env.insert("PMIX_RANK".into(), rank.to_string());
    env.entry("PMIX_SERVER_TMPDIR".into())
        .or_insert_with(|| plan.tmpdir.clone());

    if plan.num_nodes > 1 {
        let local_size = plan.local_procs.len().to_string();
        env.insert("PMIX_LOCAL_SIZE".into(), local_size.clone());
        if let Some(tasks_per_node) = plan.universe_size.checked_div(plan.num_nodes) {
            env.insert("PMIX_NODE_SIZE".into(), tasks_per_node.to_string());
        }
        if let Some(host) = plan.peer_hosts.get(plan.node_index as usize) {
            env.insert("PMIX_HOSTNAME".into(), host.clone());
        }
        env.insert("PMIX_GDS_MODULE".into(), "hash".into());
        env.insert("PMIX_NODEID".into(), plan.node_index.to_string());
        // Open MPI defaults to async modex (on-demand dmodex). Spur's embedded PMIx
        // server implements fence-based exchange only, not direct modex fetch.
        env.insert("OMPI_MCA_pmix_base_async_modex".into(), "0".into());
    }
}

/// Merge per-rank setup_fork env into task env before exec.
pub fn apply_pmix_setup_fork_env(
    host: &MpiPluginHost,
    plan: &PmixLaunchPlan,
    rank: u32,
    env: &mut HashMap<String, String>,
) -> Result<(), String> {
    let fork_env = host.pmix_setup_fork_env(plan, rank)?;
    validate_pmix_env(&fork_env)?;
    env.extend(fork_env);
    Ok(())
}

/// Per-local-rank setup_fork env for a multi-task node launch.
pub fn pmix_setup_fork_env_for_node_tasks(
    host: &MpiPluginHost,
    plan: &PmixLaunchPlan,
    task_offset: u32,
    tasks_on_node: u32,
) -> Result<Vec<HashMap<String, String>>, String> {
    let mut out = Vec::with_capacity(tasks_on_node as usize);
    for local_rank in 0..tasks_on_node {
        let rank = task_offset + local_rank;
        let rank_env = host.pmix_setup_fork_env(plan, rank)?;
        validate_pmix_env(&rank_env)?;
        out.push(rank_env);
    }
    Ok(out)
}

/// Remove launcher-level MPI/PMIx variables so a per-rank wrapper owns them.
///
/// Batch jobs inherit the submitter's full environment through the executor;
/// stale `PMI_*` or `OMPI_MCA_ess*` values can make Open MPI ignore the
/// per-rank `PMIX_*` exports from [`build_multi_task_pmix_wrapper`].
pub fn strip_launcher_mpi_env(env: &mut HashMap<String, String>) {
    env.retain(|key, _| !is_stale_launcher_mpi_env_key(key));
}

fn is_stale_launcher_mpi_env_key(key: &str) -> bool {
    key.starts_with("PMIX_")
        || key.starts_with("PMI")
        || key.starts_with("OMPI_MCA_ess")
        || matches!(
            key,
            "LOCAL_RANK"
                | "LOCAL_WORLD_SIZE"
                | "NPROC_PER_NODE"
                | "NODE_RANK"
                | "SPUR_NODE_RANK"
                | "WORLD_SIZE"
                | "RANK"
                | "MASTER_ADDR"
                | "MASTER_PORT"
                | "SPUR_PEER_NODES"
        )
}

fn apply_pmix_uri_aliases(env: &mut HashMap<String, String>) {
    let uri = env
        .get("PMIX_SERVER_URI")
        .or_else(|| env.get("PMIX_SERVER_URI4"))
        .or_else(|| env.get("PMIX_SERVER_URI41"))
        .or_else(|| env.get("PMIX_SERVER_URI3"))
        .or_else(|| env.get("PMIX_SERVER_URI2"))
        .cloned();
    let Some(uri) = uri else {
        return;
    };
    env.entry("PMIX_SERVER_URI".into())
        .or_insert_with(|| uri.clone());
    env.entry("PMIX_SERVER_URI4".into())
        .or_insert_with(|| uri.clone());
    env.entry("PMIX_SERVER_URI3".into()).or_insert(uri);
}

fn parse_setup_fork_env(env: *mut *mut c_char) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if env.is_null() {
        return out;
    }
    let mut cur = env;
    unsafe {
        while !(*cur).is_null() {
            let entry = CStr::from_ptr(*cur).to_string_lossy();
            if let Some((key, value)) = entry.split_once('=') {
                if !key.is_empty() {
                    out.insert(key.to_string(), value.to_string());
                }
            }
            cur = cur.add(1);
        }
    }
    out
}

fn validate_pmix_env(env: &HashMap<String, String>) -> Result<(), String> {
    for key in PMIX_ENV_KEYS {
        match env.get(*key) {
            Some(value) if !value.is_empty() => {}
            _ => return Err(format!("missing PMIx env {key}")),
        }
    }
    Ok(())
}

fn plan_to_c(plan: &PmixLaunchPlan) -> Result<SpurMpiLaunchPlan, String> {
    let mut c_plan = SpurMpiLaunchPlan {
        job_id: plan.job_id,
        step_id: plan.step_id,
        namespace: [0; 256],
        universe_size: plan.universe_size,
        task_offset: plan.task_offset,
        num_local_procs: plan.local_procs.len() as c_uint,
        local_procs: [SpurMpiProc {
            rank: 0,
            local_rank: 0,
        }; 256],
        tmpdir: [0; 512],
        job_uid: plan.job_uid,
        job_gid: plan.job_gid,
        num_nodes: plan.num_nodes,
        node_index: plan.node_index,
        num_peer_hosts: plan.peer_hosts.len() as c_uint,
        peer_hosts: [[0; 256]; 64],
        modex_connect_timeout_sec: plan.modex_connect_timeout_secs,
        modex_fence_timeout_sec: plan.modex_fence_timeout_secs,
        modex_verify_timeout_sec: plan.modex_verify_timeout_secs,
    };
    write_c_str(&mut c_plan.namespace, &plan.namespace)?;
    write_c_str(&mut c_plan.tmpdir, &plan.tmpdir)?;
    for (idx, host) in plan.peer_hosts.iter().enumerate() {
        if idx >= 64 {
            return Err("peer_hosts exceeds plugin max (64)".into());
        }
        write_c_str(&mut c_plan.peer_hosts[idx], host)?;
    }
    for (idx, proc) in plan.local_procs.iter().enumerate() {
        c_plan.local_procs[idx] = SpurMpiProc {
            rank: proc.rank,
            local_rank: proc.local_rank,
        };
    }
    Ok(c_plan)
}

fn write_c_str(dest: &mut [c_char], value: &str) -> Result<(), String> {
    if dest.is_empty() {
        return Ok(());
    }
    let bytes = value.as_bytes();
    let limit = dest.len().saturating_sub(1);
    if bytes.len() > limit {
        return Err(format!("string exceeds max length {limit}"));
    }
    for (idx, byte) in bytes.iter().enumerate() {
        dest[idx] = *byte as c_char;
    }
    dest[bytes.len()] = 0;
    Ok(())
}

fn c_str_to_string(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

pub fn plan_from_proto(
    proto: &spur_proto::proto::PmixLaunchPlan,
) -> Result<PmixLaunchPlan, String> {
    let plan = PmixLaunchPlan {
        job_id: proto.job_id,
        step_id: proto.step_id,
        namespace: if proto.namespace.is_empty() {
            PmixLaunchPlan::namespace_for_step(proto.job_id, proto.step_id)
        } else {
            proto.namespace.clone()
        },
        universe_size: proto.universe_size,
        task_offset: proto.task_offset,
        local_procs: proto
            .local_procs
            .iter()
            .map(|proc| mpi::PmixLocalProc {
                rank: proc.rank,
                local_rank: proc.local_rank,
            })
            .collect(),
        tmpdir: proto.tmpdir.clone(),
        job_uid: proto.job_uid,
        job_gid: proto.job_gid,
        num_nodes: proto.num_nodes.max(1),
        node_index: proto.node_index,
        peer_hosts: proto.peer_hosts.clone(),
        modex_connect_timeout_secs: proto.modex_connect_timeout_secs,
        modex_fence_timeout_secs: proto.modex_fence_timeout_secs,
        modex_verify_timeout_secs: proto.modex_verify_timeout_secs,
    };
    mpi::validate_pmix_plan(&plan)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_STEP: StepId = spur_core::step::STEP_BATCH;

    #[test]
    fn plan_credentials_survive_proto_roundtrip_and_plan_to_c() {
        let plan = PmixLaunchPlan::local_tasks(
            7,
            TEST_STEP,
            4,
            0,
            4,
            "/tmp/pmix",
            1001,
            1002,
            1,
            0,
            vec![],
        );
        let proto = mpi::plan_to_proto(plan);
        let restored = plan_from_proto(&proto).unwrap();
        assert_eq!(restored.job_uid, 1001);
        assert_eq!(restored.job_gid, 1002);
        let c = plan_to_c(&restored).unwrap();
        assert_eq!(c.job_uid, 1001);
        assert_eq!(c.job_gid, 1002);
    }

    #[test]
    fn missing_plugin_returns_actionable_error() {
        let host = MpiPluginHost::new(MpiConfig {
            plugin_dir: "/nonexistent/spur/plugins".into(),
            ..MpiConfig::default()
        });
        let plan =
            PmixLaunchPlan::local_tasks(1, TEST_STEP, 1, 0, 1, "/tmp/pmix", 0, 0, 1, 0, vec![]);
        let err = host.start_pmix_server(&plan).unwrap_err();
        assert!(err.contains("MPI plugin not found"));
    }

    #[test]
    fn validate_pmix_env_requires_all_keys() {
        let mut env = HashMap::new();
        env.insert("PMIX_SERVER_URI".into(), "pmixsrv".into());
        assert!(validate_pmix_env(&env).is_err());

        for key in PMIX_ENV_KEYS {
            env.insert(key.to_string(), "x".into());
        }
        validate_pmix_env(&env).unwrap();
    }

    #[test]
    fn start_rejects_more_than_256_local_procs() {
        let host = MpiPluginHost::new(MpiConfig::default());
        let plan = PmixLaunchPlan {
            job_id: 1,
            step_id: TEST_STEP,
            namespace: "spur.1".into(),
            universe_size: 300,
            task_offset: 0,
            local_procs: (0..257)
                .map(|rank| mpi::PmixLocalProc {
                    rank,
                    local_rank: rank,
                })
                .collect(),
            tmpdir: "/tmp/pmix".into(),
            job_uid: 0,
            job_gid: 0,
            num_nodes: 1,
            node_index: 0,
            peer_hosts: vec![],
            modex_connect_timeout_secs: 0,
            modex_fence_timeout_secs: 0,
            modex_verify_timeout_secs: 0,
        };
        let err = host.start_pmix_server(&plan).unwrap_err();
        assert!(err.contains("max 256"));
    }

    #[test]
    fn a_second_start_for_one_step_is_refused() {
        let host = MpiPluginHost::new(MpiConfig::default());
        host.active_namespaces
            .lock()
            .unwrap()
            .insert((6, TEST_STEP), "spur.6.4294967294".into());
        let plan =
            PmixLaunchPlan::local_tasks(6, TEST_STEP, 1, 0, 1, "/tmp/pmix", 0, 0, 1, 0, vec![]);

        let err = host.start_pmix_server(&plan).unwrap_err();

        assert!(err.contains("already running"), "{err}");
    }
    #[test]
    fn a_failed_start_registers_nothing() {
        let host = MpiPluginHost::new(MpiConfig {
            plugin_dir: "/nonexistent/spur/plugins".into(),
            ..MpiConfig::default()
        });
        let plan =
            PmixLaunchPlan::local_tasks(5, TEST_STEP, 1, 0, 1, "/tmp/pmix", 0, 0, 1, 0, vec![]);

        assert!(host.start_pmix_server(&plan).is_err());

        assert!(!host.has_active_pmix(5, TEST_STEP));
    }
    #[test]
    fn write_c_str_rejects_overlong_value() {
        let mut dest = [0i8; 8];
        assert!(write_c_str(&mut dest, "1234567").is_ok());
        assert!(write_c_str(&mut dest, "12345678").is_err());
    }

    #[test]
    fn write_c_str_noop_on_empty_dest() {
        let mut dest: [c_char; 0] = [];
        write_c_str(&mut dest, "hello").unwrap();
    }

    #[test]
    fn apply_pmix_uri_aliases_fills_missing_uri_from_uri4() {
        let mut env = HashMap::from([("PMIX_SERVER_URI4".into(), "pmix://host:1234".into())]);
        apply_pmix_uri_aliases(&mut env);
        assert_eq!(
            env.get("PMIX_SERVER_URI").map(String::as_str),
            Some("pmix://host:1234")
        );
        assert_eq!(
            env.get("PMIX_SERVER_URI3").map(String::as_str),
            Some("pmix://host:1234")
        );
    }

    #[test]
    fn normalize_pmix_fork_env_aligns_multi_node_hostname_and_sizes() {
        let plan = PmixLaunchPlan::local_tasks(
            9,
            TEST_STEP,
            4,
            2,
            2,
            "/tmp/pmix",
            0,
            0,
            2,
            1,
            vec!["node-a.example.com".into(), "node-b.example.com".into()],
        );
        let mut env = HashMap::from([
            ("PMIX_HOSTNAME".into(), "node-b".into()),
            ("PMIX_APP_SIZE".into(), "2".into()),
            ("PMIX_GDS_MODULE".into(), "shmem,hash".into()),
        ]);
        normalize_pmix_fork_env(&mut env, &plan, 2);
        assert_eq!(
            env.get("PMIX_HOSTNAME").map(String::as_str),
            Some("node-b.example.com")
        );
        assert_eq!(env.get("PMIX_APP_SIZE").map(String::as_str), Some("4"));
        assert_eq!(env.get("PMIX_NODE_SIZE").map(String::as_str), Some("2"));
        assert_eq!(env.get("PMIX_GDS_MODULE").map(String::as_str), Some("hash"));
        assert_eq!(env.get("PMIX_NODEID").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("OMPI_MCA_pmix_base_async_modex")
                .map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn parse_setup_fork_env_splits_key_value_pairs() {
        let entries = [
            CString::new("PMIX_RANK=2").unwrap(),
            CString::new("PMIX_SERVER_URI=pmix://x").unwrap(),
        ];
        let mut ptrs: Vec<*mut c_char> = entries
            .iter()
            .map(|s| s.as_ptr() as *mut c_char)
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect();
        let env = parse_setup_fork_env(ptrs.as_mut_ptr());
        assert_eq!(env.get("PMIX_RANK").map(String::as_str), Some("2"));
        assert_eq!(
            env.get("PMIX_SERVER_URI").map(String::as_str),
            Some("pmix://x")
        );
    }

    #[test]
    fn pmix_launch_guard_start_failure_leaves_no_active_namespace() {
        let host = Arc::new(MpiPluginHost::new(MpiConfig {
            plugin_dir: "/nonexistent/spur/plugins".into(),
            ..MpiConfig::default()
        }));
        let plan =
            PmixLaunchPlan::local_tasks(9, TEST_STEP, 1, 0, 1, "/tmp/pmix", 0, 0, 1, 0, vec![]);
        assert!(PmixLaunchGuard::start(host.clone(), &plan).is_err());
        assert!(!host.has_active_pmix(plan.job_id, plan.step_id));
    }

    #[test]
    fn release_stops_the_step_server_and_forgets_it() {
        let host = MpiPluginHost::new(MpiConfig::default());
        host.active_namespaces
            .lock()
            .unwrap()
            .insert((3, TEST_STEP), "spur.3.4294967294".into());

        host.release_pmix_server(3, TEST_STEP).unwrap();

        assert!(!host.has_active_pmix(3, TEST_STEP));
        assert!(host.release_pmix_server(3, TEST_STEP).is_err());
    }

    #[test]
    fn stop_pmix_server_clears_entry_even_when_plugin_unloaded() {
        let host = MpiPluginHost::new(MpiConfig::default());
        host.active_namespaces
            .lock()
            .unwrap()
            .insert((4, TEST_STEP), "spur.4".into());
        host.stop_pmix_job(4).unwrap();
        assert!(!host.has_active_pmix(4, TEST_STEP));
    }

    #[test]
    fn release_evicts_namespace_when_stop_fails() {
        let host = MpiPluginHost::new(MpiConfig::default());
        host.active_namespaces
            .lock()
            .unwrap()
            .insert((5, TEST_STEP), "bad\0namespace".into());
        assert!(host.release_pmix_server(5, TEST_STEP).is_err());
        assert!(!host.has_active_pmix(5, TEST_STEP));
    }

    #[test]
    fn stop_pmix_server_evicts_namespace_when_stop_fails() {
        let host = MpiPluginHost::new(MpiConfig::default());
        host.active_namespaces
            .lock()
            .unwrap()
            .insert((6, TEST_STEP), "bad\0namespace".into());
        assert!(host.stop_pmix_job(6).is_err());
        assert!(!host.has_active_pmix(6, TEST_STEP));
    }

    #[test]
    fn strip_launcher_mpi_env_removes_stale_launcher_keys() {
        let mut env = HashMap::from([
            ("PMIX_RANK".into(), "0".into()),
            ("PMIX_SIZE".into(), "1".into()),
            ("PMI_RANK".into(), "0".into()),
            ("PMI_SIZE".into(), "1".into()),
            ("OMPI_MCA_ess".into(), "singleton".into()),
            ("LOCAL_RANK".into(), "0".into()),
            ("LOCAL_WORLD_SIZE".into(), "4".into()),
            ("NODE_RANK".into(), "0".into()),
            ("PATH".into(), "/usr/bin".into()),
            ("HOME".into(), "/home/user".into()),
        ]);
        strip_launcher_mpi_env(&mut env);
        assert_eq!(
            env,
            HashMap::from([
                ("PATH".into(), "/usr/bin".into()),
                ("HOME".into(), "/home/user".into()),
            ])
        );
    }

    #[test]
    fn releasing_a_server_this_process_never_hosted_is_an_error() {
        let host = MpiPluginHost::new(MpiConfig::default());

        let err = host.release_pmix_server(99, TEST_STEP).unwrap_err();

        assert!(err.contains("no PMIx server registered"), "{err}");
    }

    #[test]
    fn stopping_a_job_clears_every_step_it_hosts_and_leaves_other_jobs_alone() {
        let host = MpiPluginHost::new(MpiConfig::default());
        for step_id in [TEST_STEP, 0, 1] {
            host.active_namespaces
                .lock()
                .unwrap()
                .insert((55, step_id), format!("spur.55.{step_id}"));
        }
        host.active_namespaces
            .lock()
            .unwrap()
            .insert((56, TEST_STEP), "spur.56.4294967294".into());

        host.stop_pmix_job(55).unwrap();

        for step_id in [TEST_STEP, 0, 1] {
            assert!(!host.has_active_pmix(55, step_id));
        }
        assert!(host.has_active_pmix(56, TEST_STEP));
    }

    #[test]
    fn plan_modex_timeouts_survive_proto_roundtrip() {
        let plan = PmixLaunchPlan::local_tasks(
            91,
            TEST_STEP,
            4,
            0,
            2,
            "/tmp/pmix",
            0,
            0,
            2,
            0,
            vec!["10.0.0.1".into(), "10.0.0.2".into()],
        )
        .with_modex_timeouts(7, 90, 15);
        let proto = mpi::plan_to_proto(plan);
        assert_eq!(proto.modex_connect_timeout_secs, 7);
        assert_eq!(proto.modex_fence_timeout_secs, 90);
        assert_eq!(proto.modex_verify_timeout_secs, 15);
        let restored = plan_from_proto(&proto).unwrap();
        assert_eq!(restored.modex_connect_timeout_secs, 7);
        assert_eq!(restored.modex_fence_timeout_secs, 90);
        assert_eq!(restored.modex_verify_timeout_secs, 15);
    }

    #[test]
    #[ignore = "requires SPUR_TEST_MPI_PLUGIN pointing at a built spur_mpi_pmix.so"]
    fn pmix_launch_guard_drop_rolls_back_after_successful_start() {
        let plugin_path = std::env::var("SPUR_TEST_MPI_PLUGIN")
            .expect("SPUR_TEST_MPI_PLUGIN must be set when running ignored PMIx plugin tests");
        assert!(
            std::path::Path::new(&plugin_path).is_file(),
            "SPUR_TEST_MPI_PLUGIN must point at an existing plugin: {plugin_path}"
        );

        let host = Arc::new(MpiPluginHost::new(MpiConfig {
            pmix_plugin: plugin_path,
            pmix_tmpdir: "/tmp/spur-pmix-test".into(),
            ..MpiConfig::default()
        }));
        let plan = PmixLaunchPlan::local_tasks(
            7777,
            TEST_STEP,
            1,
            0,
            1,
            "/tmp/spur-pmix-test",
            0,
            0,
            1,
            0,
            vec![],
        );
        {
            let guard = PmixLaunchGuard::start(host.clone(), &plan).expect("plugin start");
            assert!(host.has_active_pmix(plan.job_id, plan.step_id));
            drop(guard);
        }
        assert!(!host.has_active_pmix(plan.job_id, plan.step_id));
    }
}
