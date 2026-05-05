//! SPANK plugin host.
//!
//! Loads existing Slurm SPANK plugins (.so files) via dlopen and provides
//! the spank_* callback API (11 hooks, ~12 API functions).

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

/// SPANK callback hook points (matches Slurm's spank.h).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpankHook {
    Init,
    InitPost,
    LocalUserInit,
    UserInit,
    TaskInit,
    TaskInitPrivileged,
    TaskPost,
    TaskExit,
    JobEpilog,
    SlurmctldExit,
    Exit,
}

impl SpankHook {
    /// C symbol name for this hook.
    pub fn symbol_name(&self) -> &'static str {
        match self {
            Self::Init => "slurm_spank_init",
            Self::InitPost => "slurm_spank_init_post_opt",
            Self::LocalUserInit => "slurm_spank_local_user_init",
            Self::UserInit => "slurm_spank_user_init",
            Self::TaskInit => "slurm_spank_task_init",
            Self::TaskInitPrivileged => "slurm_spank_task_init_privileged",
            Self::TaskPost => "slurm_spank_task_post_fork",
            Self::TaskExit => "slurm_spank_task_exit",
            Self::JobEpilog => "slurm_spank_job_epilog",
            Self::SlurmctldExit => "slurm_spank_slurmd_exit",
            Self::Exit => "slurm_spank_exit",
        }
    }
}

/// SPANK item IDs for spank_get_item (10 common items).
#[repr(C)]
pub enum SpankItem {
    JobId = 0,
    JobUid = 1,
    JobGid = 2,
    JobStepId = 3,
    JobNnodes = 4,
    JobNodeid = 5,
    JobLocalTaskCount = 6,
    JobTotalTaskCount = 7,
    JobArgv = 8,
    TaskPid = 9,
}

/// A loaded SPANK plugin.
struct SpankPlugin {
    path: PathBuf,
    #[cfg(unix)]
    lib: libloading::Library,
    name: String,
}

/// The SPANK plugin host — manages loading and invoking plugins.
pub struct SpankHost {
    plugins: Vec<SpankPlugin>,
    /// Current job context for spank_get_item.
    context: SpankContext,
}

/// Job context available to SPANK plugins.
#[derive(Default, Clone)]
pub struct SpankContext {
    pub job_id: u32,
    pub uid: u32,
    pub gid: u32,
    pub step_id: u32,
    pub num_nodes: u32,
    pub node_id: u32,
    pub local_task_count: u32,
    pub total_task_count: u32,
    pub task_pid: u32,
}

impl SpankHost {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            context: SpankContext::default(),
        }
    }

    /// Load a SPANK plugin from a .so file.
    #[cfg(unix)]
    pub fn load_plugin(&mut self, path: &Path) -> anyhow::Result<()> {
        use anyhow::Context;

        let lib = unsafe {
            libloading::Library::new(path)
                .with_context(|| format!("failed to dlopen {}", path.display()))?
        };

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        info!(plugin = %name, path = %path.display(), "loaded SPANK plugin");

        self.plugins.push(SpankPlugin {
            path: path.to_path_buf(),
            lib,
            name,
        });

        Ok(())
    }

    /// Not available on non-unix platforms.
    #[cfg(not(unix))]
    pub fn load_plugin(&mut self, path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("SPANK plugins only supported on Unix");
    }

    /// Set the job context for subsequent hook calls.
    pub fn set_context(&mut self, ctx: SpankContext) {
        self.context = ctx;
    }

    /// Invoke a hook across all loaded plugins.
    pub fn invoke_hook(&self, hook: SpankHook) -> Result<(), SpankError> {
        let symbol = hook.symbol_name();

        let mut handle = SpankHandle {
            context: self.context.clone(),
            env: HashMap::new(),
        };
        let handle_ptr = &mut handle as *mut SpankHandle;

        for plugin in &self.plugins {
            #[cfg(unix)]
            {
                // Look up the symbol
                let func: Result<
                    libloading::Symbol<
                        unsafe extern "C" fn(*mut SpankHandle, c_int, *mut *mut c_char) -> c_int,
                    >,
                    _,
                > = unsafe { plugin.lib.get(symbol.as_bytes()) };

                match func {
                    Ok(f) => {
                        debug!(plugin = %plugin.name, path = %plugin.path.display(), hook = symbol, "invoking SPANK hook");
                        let rc = unsafe { f(handle_ptr, 0, std::ptr::null_mut()) };
                        if rc != 0 {
                            warn!(
                                plugin = %plugin.name,
                                path = %plugin.path.display(),
                                hook = symbol,
                                rc,
                                "SPANK hook returned error"
                            );
                            return Err(SpankError::HookFailed {
                                plugin: plugin.name.clone(),
                                hook: symbol.to_string(),
                                rc,
                            });
                        }
                    }
                    Err(_) => {
                        // Plugin doesn't implement this hook — that's fine
                        debug!(
                            plugin = %plugin.name,
                            path = %plugin.path.display(),
                            hook = symbol,
                            "SPANK hook not found, skipping"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Number of loaded plugins.
    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }
}

/// Handle passed to SPANK plugin callbacks, providing access to job
/// context and a per-invocation environment variable map.
#[repr(C)]
pub struct SpankHandle {
    pub context: SpankContext,
    pub env: HashMap<String, String>,
}

/// Retrieve a job context item from the SPANK handle.
///
/// The `item` parameter corresponds to `SpankItem` variants (0=JobId,
/// 1=JobUid, etc.).  On success the value is written through `val` and 0
/// is returned; on error -1 is returned.
#[no_mangle]
pub extern "C" fn spur_spank_get_item(
    handle: *mut SpankHandle,
    item: c_int,
    val: *mut *mut std::ffi::c_void,
) -> c_int {
    if handle.is_null() || val.is_null() {
        return -1;
    }
    let handle = unsafe { &*handle };
    match item {
        0 => {
            // SPANK_JOB_ID
            unsafe {
                *(val as *mut u32) = handle.context.job_id;
            }
            0
        }
        1 => {
            // SPANK_JOB_UID
            unsafe {
                *(val as *mut u32) = handle.context.uid;
            }
            0
        }
        2 => {
            // SPANK_JOB_GID
            unsafe {
                *(val as *mut u32) = handle.context.gid;
            }
            0
        }
        3 => {
            // SPANK_JOB_STEPID
            unsafe {
                *(val as *mut u32) = handle.context.step_id;
            }
            0
        }
        4 => {
            // SPANK_JOB_NNODES
            unsafe {
                *(val as *mut u32) = handle.context.num_nodes;
            }
            0
        }
        5 => {
            // SPANK_JOB_NODEID
            unsafe {
                *(val as *mut u32) = handle.context.node_id;
            }
            0
        }
        6 => {
            // SPANK_JOB_LOCAL_TASK_COUNT
            unsafe {
                *(val as *mut u32) = handle.context.local_task_count;
            }
            0
        }
        7 => {
            // SPANK_JOB_TOTAL_TASK_COUNT
            unsafe {
                *(val as *mut u32) = handle.context.total_task_count;
            }
            0
        }
        // 8 = SPANK_JOB_ARGV — not yet implemented (requires pointer-to-array)
        9 => {
            // SPANK_TASK_PID
            unsafe {
                *(val as *mut u32) = handle.context.task_pid;
            }
            0
        }
        _ => -1,
    }
}

/// Set an environment variable in the SPANK handle's per-invocation map.
///
/// Returns 0 on success, -1 if any pointer is null.
#[no_mangle]
pub extern "C" fn spur_spank_set_var(
    handle: *mut SpankHandle,
    key: *const c_char,
    val: *const c_char,
) -> c_int {
    if handle.is_null() || key.is_null() || val.is_null() {
        return -1;
    }
    let handle = unsafe { &mut *handle };
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy().to_string();
    let val = unsafe { CStr::from_ptr(val) }.to_string_lossy().to_string();
    handle.env.insert(key, val);
    0
}

#[derive(Debug, thiserror::Error)]
pub enum SpankError {
    #[error("SPANK hook {hook} in plugin {plugin} returned {rc}")]
    HookFailed {
        plugin: String,
        hook: String,
        rc: c_int,
    },
    #[error("plugin load failed: {0}")]
    LoadFailed(String),
}

/// Parse plugstack.conf (SPANK config file).
///
/// Format: `required|optional <plugin.so> [args...]`
pub fn parse_plugstack(path: &Path) -> anyhow::Result<Vec<PlugstackEntry>> {
    let content = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, char::is_whitespace).collect();
        if parts.len() < 2 {
            continue;
        }

        let required = parts[0] == "required";
        let plugin_path = PathBuf::from(parts[1]);
        let args: Vec<String> = parts
            .get(2)
            .map(|a| a.split_whitespace().map(String::from).collect())
            .unwrap_or_default();

        entries.push(PlugstackEntry {
            required,
            path: plugin_path,
            args,
        });
    }

    Ok(entries)
}

pub struct PlugstackEntry {
    pub required: bool,
    pub path: PathBuf,
    pub args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_spank_host_new() {
        let host = SpankHost::new();
        assert_eq!(host.plugin_count(), 0);
    }

    #[test]
    fn test_hook_symbol_names() {
        assert_eq!(SpankHook::Init.symbol_name(), "slurm_spank_init");
        assert_eq!(SpankHook::TaskExit.symbol_name(), "slurm_spank_task_exit");
    }

    #[test]
    fn test_spank_hook_all_symbol_names() {
        // Verify all hook symbol names match Slurm convention
        assert_eq!(SpankHook::Init.symbol_name(), "slurm_spank_init");
        assert_eq!(
            SpankHook::InitPost.symbol_name(),
            "slurm_spank_init_post_opt"
        );
        assert_eq!(
            SpankHook::LocalUserInit.symbol_name(),
            "slurm_spank_local_user_init"
        );
        assert_eq!(SpankHook::UserInit.symbol_name(), "slurm_spank_user_init");
        assert_eq!(SpankHook::TaskInit.symbol_name(), "slurm_spank_task_init");
        assert_eq!(
            SpankHook::TaskInitPrivileged.symbol_name(),
            "slurm_spank_task_init_privileged"
        );
        assert_eq!(
            SpankHook::TaskPost.symbol_name(),
            "slurm_spank_task_post_fork"
        );
        assert_eq!(SpankHook::TaskExit.symbol_name(), "slurm_spank_task_exit");
        assert_eq!(SpankHook::JobEpilog.symbol_name(), "slurm_spank_job_epilog");
        assert_eq!(
            SpankHook::SlurmctldExit.symbol_name(),
            "slurm_spank_slurmd_exit"
        );
        assert_eq!(SpankHook::Exit.symbol_name(), "slurm_spank_exit");
    }

    #[test]
    fn test_spank_host_empty_invoke() {
        let host = SpankHost::new();
        // Invoking hooks on empty host should succeed (no plugins to fail)
        assert!(host.invoke_hook(SpankHook::Init).is_ok());
        assert!(host.invoke_hook(SpankHook::TaskExit).is_ok());
        assert!(host.invoke_hook(SpankHook::JobEpilog).is_ok());
    }

    #[test]
    fn test_plugstack_parse_missing_file() {
        let result = parse_plugstack(Path::new("/nonexistent/plugstack.conf"));
        assert!(result.is_err());
    }

    #[test]
    fn test_plugstack_parse_valid() {
        let dir = std::env::temp_dir().join("spur_spank_test");
        let _ = std::fs::create_dir_all(&dir);
        let conf_path = dir.join("plugstack.conf");
        let mut f = std::fs::File::create(&conf_path).unwrap();
        writeln!(f, "# comment line").unwrap();
        writeln!(f, "required /usr/lib/spank/plugin1.so arg1 arg2").unwrap();
        writeln!(f, "optional /usr/lib/spank/plugin2.so").unwrap();
        writeln!(f, "").unwrap();
        drop(f);

        let entries = parse_plugstack(&conf_path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].required);
        assert_eq!(entries[0].path, PathBuf::from("/usr/lib/spank/plugin1.so"));
        assert_eq!(entries[0].args, vec!["arg1", "arg2"]);
        assert!(!entries[1].required);
        assert_eq!(entries[1].path, PathBuf::from("/usr/lib/spank/plugin2.so"));
        assert!(entries[1].args.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_spank_context_default() {
        let ctx = SpankContext::default();
        assert_eq!(ctx.job_id, 0);
        assert_eq!(ctx.uid, 0);
        assert_eq!(ctx.task_pid, 0);
    }

    #[test]
    fn test_spank_host_set_context() {
        let mut host = SpankHost::new();
        host.set_context(SpankContext {
            job_id: 42,
            uid: 1000,
            gid: 1000,
            step_id: 0,
            num_nodes: 1,
            node_id: 0,
            local_task_count: 1,
            total_task_count: 1,
            task_pid: 12345,
        });
        // Context is set without panicking
        assert_eq!(host.plugin_count(), 0);
    }
}
