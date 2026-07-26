// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runtime-loaded PMIx plugin host for spurd.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use libloading::{Library, Symbol};
use spur_core::config::MpiConfig;
use spur_core::mpi::{self, PmixLaunchPlan};
use tracing::{debug, info, warn};

const PMIX_ENV_KEYS: &[&str] = &[
    "PMIX_SERVER_URI",
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
    namespace: [c_char; 256],
    universe_size: c_uint,
    task_offset: c_uint,
    num_local_procs: c_uint,
    local_procs: [SpurMpiProc; 256],
    tmpdir: [c_char; 512],
}

type VersionFn = unsafe extern "C" fn() -> c_int;
type RuntimeVersionFn = unsafe extern "C" fn(*mut c_char, usize) -> c_int;
type ServerStartFn =
    unsafe extern "C" fn(*const SpurMpiLaunchPlan, *mut c_char, usize) -> c_int;
type ServerStopFn = unsafe extern "C" fn(*const c_char, *mut c_char, usize) -> c_int;
type EnvFn = unsafe extern "C" fn(*const SpurMpiLaunchPlan, c_uint, *const c_char, *mut c_char, usize) -> c_int;

struct PluginApi {
    _library: Library,
    server_start: ServerStartFn,
    server_stop: ServerStopFn,
    env: EnvFn,
}

pub struct MpiPluginHost {
    config: MpiConfig,
    plugin: Mutex<Option<PluginApi>>,
    active_namespaces: Mutex<HashMap<u32, String>>,
}

/// Rolls back PMIx server registration when launch fails before the job is committed.
pub struct PmixLaunchGuard {
    host: Arc<MpiPluginHost>,
    job_id: u32,
    rollback: bool,
}

impl PmixLaunchGuard {
    pub fn start(host: Arc<MpiPluginHost>, plan: &PmixLaunchPlan) -> Result<Self, String> {
        host.start_pmix_server(plan)?;
        Ok(Self {
            host,
            job_id: plan.job_id,
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
        if let Err(err) = self.host.stop_pmix_server(self.job_id) {
            warn!(job_id = self.job_id, error = %err, "PMIx rollback stop failed");
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

    pub fn plugin_path(&self) -> PathBuf {
        self.config.resolve_pmix_plugin_path()
    }

    pub fn has_active_pmix(&self, job_id: u32) -> bool {
        self.active_namespaces
            .lock()
            .ok()
            .is_some_and(|guard| guard.contains_key(&job_id))
    }

    fn load_plugin(&self) -> Result<(), String> {
        let mut guard = self.plugin.lock().map_err(|_| "plugin lock poisoned".to_string())?;
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
        let env: Symbol<EnvFn> = unsafe { library.get(b"spur_mpi_pmix_env") }
            .map_err(|e| format!("MPI plugin missing spur_mpi_pmix_env: {e}"))?;

        let api_version = unsafe { version() };
        if api_version != 1 {
            return Err(format!(
                "unsupported MPI plugin API version {api_version} (expected 1)"
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
        let env_fn = *env;

        *guard = Some(PluginApi {
            _library: library,
            server_start: server_start_fn,
            server_stop: server_stop_fn,
            env: env_fn,
        });
        Ok(())
    }

    pub fn start_pmix_server(&self, plan: &PmixLaunchPlan) -> Result<(), String> {
        if plan.local_procs.len() > 256 {
            return Err(format!(
                "PMIx launch plan has {} local procs (max 256)",
                plan.local_procs.len()
            ));
        }
        self.load_plugin()?;
        let c_plan = plan_to_c(plan);
        let mut errbuf = vec![0i8; 512];
        let rc = {
            let guard = self.plugin.lock().map_err(|_| "plugin lock poisoned".to_string())?;
            let api = guard.as_ref().ok_or_else(|| "MPI plugin not loaded".to_string())?;
            unsafe { (api.server_start)(&c_plan, errbuf.as_mut_ptr(), errbuf.len()) }
        };
        if rc != 0 {
            let err = c_str_to_string(&errbuf);
            warn!(
                job_id = plan.job_id,
                namespace = %plan.namespace,
                error = %err,
                "PMIx server start failed"
            );
            return Err(err);
        }
        self.active_namespaces
            .lock()
            .map_err(|_| "namespace lock poisoned".to_string())?
            .insert(plan.job_id, plan.namespace.clone());
        info!(
            job_id = plan.job_id,
            namespace = %plan.namespace,
            universe_size = plan.universe_size,
            local_procs = plan.local_procs.len(),
            "PMIx server started"
        );
        Ok(())
    }

    pub fn stop_pmix_server(&self, job_id: u32) -> Result<(), String> {
        let namespace = self
            .active_namespaces
            .lock()
            .map_err(|_| "namespace lock poisoned".to_string())?
            .get(&job_id)
            .cloned();
        let Some(namespace) = namespace else {
            return Ok(());
        };
        if self
            .plugin
            .lock()
            .map_err(|_| "plugin lock poisoned".to_string())?
            .is_none()
        {
            return Ok(());
        }
        let c_namespace =
            CString::new(namespace.clone()).map_err(|_| "invalid PMIx namespace".to_string())?;
        let mut errbuf = vec![0i8; 256];
        let rc = {
            let guard = self
                .plugin
                .lock()
                .map_err(|_| "plugin lock poisoned".to_string())?;
            let api = guard
                .as_ref()
                .ok_or_else(|| "MPI plugin not loaded".to_string())?;
            unsafe {
                (api.server_stop)(
                    c_namespace.as_ptr(),
                    errbuf.as_mut_ptr(),
                    errbuf.len(),
                )
            }
        };
        if rc != 0 {
            let err = c_str_to_string(&errbuf);
            warn!(job_id, namespace = %namespace, error = %err, "PMIx server stop failed");
            return Err(err);
        }
        self.active_namespaces
            .lock()
            .map_err(|_| "namespace lock poisoned".to_string())?
            .remove(&job_id);
        info!(job_id, namespace = %namespace, "PMIx server stopped");
        Ok(())
    }

    pub fn pmix_env_for_rank(
        &self,
        plan: &PmixLaunchPlan,
        rank: u32,
    ) -> Result<HashMap<String, String>, String> {
        self.load_plugin()?;
        let c_plan = plan_to_c(plan);
        let mut out = HashMap::new();
        for key in PMIX_ENV_KEYS {
            let c_key = CString::new(*key).map_err(|_| format!("invalid env key {key}"))?;
            let mut valbuf = vec![0i8; 4096];
            let rc = {
                let guard = self
                    .plugin
                    .lock()
                    .map_err(|_| "plugin lock poisoned".to_string())?;
                let api = guard.as_ref().ok_or_else(|| "MPI plugin not loaded".to_string())?;
                unsafe {
                    (api.env)(
                        &c_plan,
                        rank,
                        c_key.as_ptr(),
                        valbuf.as_mut_ptr(),
                        valbuf.len(),
                    )
                }
            };
            if rc == 0 {
                let value = c_str_to_string(&valbuf);
                if !value.is_empty() {
                    out.insert(key.to_string(), value);
                }
            } else {
                debug!(
                    job_id = plan.job_id,
                    rank,
                    key,
                    "PMIx env key unavailable"
                );
            }
        }
        validate_pmix_env(&out)?;
        Ok(out)
    }
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

fn plan_to_c(plan: &PmixLaunchPlan) -> SpurMpiLaunchPlan {
    let mut c_plan = SpurMpiLaunchPlan {
        job_id: plan.job_id,
        namespace: [0; 256],
        universe_size: plan.universe_size,
        task_offset: plan.task_offset,
        num_local_procs: plan.local_procs.len().min(256) as c_uint,
        local_procs: [SpurMpiProc {
            rank: 0,
            local_rank: 0,
        }; 256],
        tmpdir: [0; 512],
    };
    write_c_str(&mut c_plan.namespace, &plan.namespace);
    write_c_str(&mut c_plan.tmpdir, &plan.tmpdir);
    for (idx, proc) in plan.local_procs.iter().take(256).enumerate() {
        c_plan.local_procs[idx] = SpurMpiProc {
            rank: proc.rank,
            local_rank: proc.local_rank,
        };
    }
    c_plan
}

fn write_c_str(dest: &mut [c_char], value: &str) {
    let bytes = value.as_bytes();
    let limit = dest.len().saturating_sub(1);
    let copy_len = bytes.len().min(limit);
    for (idx, byte) in bytes.iter().take(copy_len).enumerate() {
        dest[idx] = *byte as c_char;
    }
    dest[copy_len] = 0;
}

fn c_str_to_string(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

pub fn plan_from_proto(proto: &spur_proto::proto::PmixLaunchPlan) -> PmixLaunchPlan {
    PmixLaunchPlan {
        job_id: proto.job_id,
        namespace: if proto.namespace.is_empty() {
            PmixLaunchPlan::namespace_for_job(proto.job_id)
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_plugin_returns_actionable_error() {
        let host = MpiPluginHost::new(MpiConfig {
            plugin_dir: "/nonexistent/spur/plugins".into(),
            ..MpiConfig::default()
        });
        let plan = PmixLaunchPlan::local_tasks(1, 1, 0, 1, "/tmp/pmix");
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
            namespace: "spur.1".into(),
            universe_size: 300,
            task_offset: 0,
            local_procs: (0..257)
                .map(|rank| mpi::PmixLocalProc { rank, local_rank: rank })
                .collect(),
            tmpdir: "/tmp/pmix".into(),
        };
        let err = host.start_pmix_server(&plan).unwrap_err();
        assert!(err.contains("max 256"));
    }

    #[test]
    fn pmix_launch_guard_start_failure_leaves_no_active_namespace() {
        let host = Arc::new(MpiPluginHost::new(MpiConfig {
            plugin_dir: "/nonexistent/spur/plugins".into(),
            ..MpiConfig::default()
        }));
        let plan = PmixLaunchPlan::local_tasks(9, 1, 0, 1, "/tmp/pmix");
        assert!(PmixLaunchGuard::start(host.clone(), &plan).is_err());
        assert!(!host.has_active_pmix(plan.job_id));
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
        let plan = PmixLaunchPlan::local_tasks(7777, 1, 0, 1, "/tmp/spur-pmix-test");
        {
            let guard = PmixLaunchGuard::start(host.clone(), &plan).expect("plugin start");
            assert!(host.has_active_pmix(plan.job_id));
            drop(guard);
        }
        assert!(!host.has_active_pmix(plan.job_id));
    }
}
