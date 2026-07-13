// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! SPANK plugin host.
//!
//! Loads existing Slurm SPANK plugins (.so files) via dlopen and provides
//! the `spank_*`/`slurm_*` callback API expected by `spank.h`.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};

use tracing::{debug, error, info, warn};

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

/// SPANK item IDs for `spank_get_item`.
///
/// Discriminants match Slurm's `enum spank_item` in `spank.h`; plugins pass
/// these numeric values, so the ordering must stay identical.
#[repr(C)]
pub enum SpankItem {
    JobUid = 0,
    JobGid = 1,
    JobId = 2,
    JobStepId = 3,
    JobNnodes = 4,
    JobNodeid = 5,
    JobLocalTaskCount = 6,
    JobTotalTaskCount = 7,
    JobNcpus = 8,
    JobArgv = 9,
    JobEnv = 10,
    TaskId = 11,
    TaskGlobalId = 12,
    TaskExitStatus = 13,
    TaskPid = 14,
}

/// `spank_err_t` return codes (subset of `spank.h`).
///
/// Plugins primarily check for `ESPANK_SUCCESS`; the remaining codes let
/// callers distinguish common failures.
pub const ESPANK_SUCCESS: c_int = 0;
pub const ESPANK_ERROR: c_int = 1;
pub const ESPANK_BAD_ARG: c_int = 2;
pub const ESPANK_NOT_TASK: c_int = 3;
pub const ESPANK_ENV_EXISTS: c_int = 4;
pub const ESPANK_ENV_NOEXIST: c_int = 5;
pub const ESPANK_NOSPACE: c_int = 6;
pub const ESPANK_NOT_REMOTE: c_int = 7;
pub const ESPANK_NOEXIST: c_int = 8;
pub const ESPANK_NOT_AVAIL: c_int = 10;
pub const ESPANK_NOT_LOCAL: c_int = 11;

/// A loaded SPANK plugin.
struct SpankPlugin {
    path: PathBuf,
    #[cfg(unix)]
    lib: libloading::Library,
    name: String,
    /// Plugstack arguments, passed to every hook as `argv` (owned so the
    /// pointers handed to the plugin stay valid for its lifetime).
    argv: Vec<CString>,
}

/// The SPANK plugin host — manages loading and invoking plugins.
pub struct SpankHost {
    plugins: Vec<SpankPlugin>,
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

impl Default for SpankHost {
    fn default() -> Self {
        Self::new()
    }
}

impl SpankHost {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// Load a SPANK plugin from a .so file, with its plugstack arguments.
    #[cfg(unix)]
    pub fn load_plugin(&mut self, path: &Path, args: &[String]) -> anyhow::Result<()> {
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

        // Drop args containing interior NUL rather than failing the load.
        let argv = args
            .iter()
            .filter_map(|a| match CString::new(a.as_str()) {
                Ok(c) => Some(c),
                Err(_) => {
                    warn!(plugin = %name, arg = %a, "skipping SPANK arg with interior NUL");
                    None
                }
            })
            .collect();

        info!(plugin = %name, path = %path.display(), "loaded SPANK plugin");

        self.plugins.push(SpankPlugin {
            path: path.to_path_buf(),
            lib,
            name,
            argv,
        });

        Ok(())
    }

    /// Not available on non-unix platforms.
    #[cfg(not(unix))]
    pub fn load_plugin(&mut self, path: &Path, args: &[String]) -> anyhow::Result<()> {
        anyhow::bail!("SPANK plugins only supported on Unix");
    }

    /// Invoke a hook across all loaded plugins against a caller-owned handle.
    ///
    /// The handle is threaded through every plugin (and reused across hooks by
    /// the caller), so env changes made by one plugin are visible to the next
    /// and can be read back after the call returns.
    pub fn invoke_hook(&self, hook: SpankHook, handle: &mut SpankHandle) -> Result<(), SpankError> {
        let symbol = hook.symbol_name();
        let handle_ptr = handle as *mut SpankHandle;

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
                        // Slurm passes exactly `ac` argv elements (no NULL terminator).
                        let mut argv: Vec<*mut c_char> = plugin
                            .argv
                            .iter()
                            .map(|a| a.as_ptr() as *mut c_char)
                            .collect();
                        let rc = unsafe { f(handle_ptr, argv.len() as c_int, argv.as_mut_ptr()) };
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
    /// Job-control environment (Slurm prepends `SPANK_` to these keys).
    pub job_control_env: HashMap<String, String>,
}

impl SpankHandle {
    /// Create a handle seeded with the job context and its environment.
    pub fn new(context: SpankContext, env: HashMap<String, String>) -> Self {
        Self {
            context,
            env,
            job_control_env: HashMap::new(),
        }
    }
}

/// Retrieve a job context item from the SPANK handle.
///
/// Matches Slurm's `spank_get_item(spank_t, spank_item_t, ...)`. Slurm
/// declares it variadic; stable Rust cannot define C-variadic functions, so
/// we take the single trailing result pointer directly. This is ABI
/// compatible on the SysV x86-64 varargs convention for the scalar items we
/// support (a single pointer passed in a register). Multi-argument items such
/// as `S_JOB_ARGV`/`S_JOB_ENV` and the pid-to-id lookups are not supported.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn spank_get_item(handle: *mut SpankHandle, item: c_int, val: *mut c_void) -> c_int {
    if handle.is_null() || val.is_null() {
        return ESPANK_BAD_ARG;
    }
    let handle = unsafe { &*handle };
    let value = match item {
        x if x == SpankItem::JobUid as c_int => handle.context.uid,
        x if x == SpankItem::JobGid as c_int => handle.context.gid,
        x if x == SpankItem::JobId as c_int => handle.context.job_id,
        x if x == SpankItem::JobStepId as c_int => handle.context.step_id,
        x if x == SpankItem::JobNnodes as c_int => handle.context.num_nodes,
        x if x == SpankItem::JobNodeid as c_int => handle.context.node_id,
        x if x == SpankItem::JobLocalTaskCount as c_int => handle.context.local_task_count,
        x if x == SpankItem::JobTotalTaskCount as c_int => handle.context.total_task_count,
        x if x == SpankItem::TaskPid as c_int => handle.context.task_pid,
        // Known Slurm items we don't source from the handle yet: report
        // "not available" rather than conflating with an invalid id.
        x if x == SpankItem::JobNcpus as c_int
            || x == SpankItem::JobArgv as c_int
            || x == SpankItem::JobEnv as c_int
            || x == SpankItem::TaskId as c_int
            || x == SpankItem::TaskGlobalId as c_int
            || x == SpankItem::TaskExitStatus as c_int =>
        {
            return ESPANK_NOT_AVAIL
        }
        _ => return ESPANK_BAD_ARG,
    };
    unsafe {
        *(val as *mut u32) = value;
    }
    ESPANK_SUCCESS
}

/// Set an environment variable in the job's environment.
///
/// Matches Slurm's `spank_setenv`. When `overwrite == 0` and the variable
/// already exists, returns `ESPANK_ENV_EXISTS` without modifying it.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn spank_setenv(
    handle: *mut SpankHandle,
    var: *const c_char,
    val: *const c_char,
    overwrite: c_int,
) -> c_int {
    if handle.is_null() || var.is_null() || val.is_null() {
        return ESPANK_BAD_ARG;
    }
    let handle = unsafe { &mut *handle };
    let key = unsafe { CStr::from_ptr(var) }.to_string_lossy().to_string();
    let value = unsafe { CStr::from_ptr(val) }.to_string_lossy().to_string();
    if overwrite == 0 && handle.env.contains_key(&key) {
        return ESPANK_ENV_EXISTS;
    }
    handle.env.insert(key, value);
    ESPANK_SUCCESS
}

/// Copy a job environment variable into `buf` (matches Slurm's `spank_getenv`).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn spank_getenv(
    handle: *mut SpankHandle,
    var: *const c_char,
    buf: *mut c_char,
    len: c_int,
) -> c_int {
    // spank.h defines the bad-arg bound as `len < 0` here but `len <= 0` for
    // spank_job_control_getenv; the asymmetry is intentional to match Slurm.
    if handle.is_null() || var.is_null() || buf.is_null() || len < 0 {
        return ESPANK_BAD_ARG;
    }
    let handle = unsafe { &*handle };
    let key = unsafe { CStr::from_ptr(var) }.to_string_lossy().to_string();
    copy_env_value(&handle.env, &key, buf, len)
}

/// Unset a job environment variable (matches Slurm's `spank_unsetenv`).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn spank_unsetenv(handle: *mut SpankHandle, var: *const c_char) -> c_int {
    if handle.is_null() || var.is_null() {
        return ESPANK_BAD_ARG;
    }
    let handle = unsafe { &mut *handle };
    let key = unsafe { CStr::from_ptr(var) }.to_string_lossy().to_string();
    handle.env.remove(&key);
    ESPANK_SUCCESS
}

/// Set a variable in the job-control environment (matches Slurm's
/// `spank_job_control_setenv`). Slurm prepends `SPANK_` to the stored key.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn spank_job_control_setenv(
    handle: *mut SpankHandle,
    name: *const c_char,
    value: *const c_char,
    overwrite: c_int,
) -> c_int {
    if handle.is_null() || name.is_null() || value.is_null() {
        return ESPANK_BAD_ARG;
    }
    let handle = unsafe { &mut *handle };
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let key = format!("SPANK_{name}");
    let value = unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .to_string();
    if overwrite == 0 && handle.job_control_env.contains_key(&key) {
        return ESPANK_ENV_EXISTS;
    }
    handle.job_control_env.insert(key, value);
    ESPANK_SUCCESS
}

/// Copy a job-control environment variable into `buf` (matches Slurm's
/// `spank_job_control_getenv`).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn spank_job_control_getenv(
    handle: *mut SpankHandle,
    name: *const c_char,
    buf: *mut c_char,
    len: c_int,
) -> c_int {
    if handle.is_null() || name.is_null() || buf.is_null() || len <= 0 {
        return ESPANK_BAD_ARG;
    }
    let handle = unsafe { &*handle };
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let key = format!("SPANK_{name}");
    copy_env_value(&handle.job_control_env, &key, buf, len)
}

/// Unset a job-control environment variable (matches Slurm's
/// `spank_job_control_unsetenv`).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn spank_job_control_unsetenv(
    handle: *mut SpankHandle,
    name: *const c_char,
) -> c_int {
    if handle.is_null() || name.is_null() {
        return ESPANK_BAD_ARG;
    }
    let handle = unsafe { &mut *handle };
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    handle.job_control_env.remove(&format!("SPANK_{name}"));
    ESPANK_SUCCESS
}

/// Return a static string for a `spank_err_t` code (matches `spank_strerror`).
#[no_mangle]
pub extern "C" fn spank_strerror(err: c_int) -> *const c_char {
    let s: &CStr = match err {
        ESPANK_SUCCESS => c"Success",
        ESPANK_BAD_ARG => c"Bad argument",
        ESPANK_NOT_TASK => c"Not in task context",
        ESPANK_ENV_EXISTS => c"Environment variable exists",
        ESPANK_ENV_NOEXIST => c"No such environment variable",
        ESPANK_NOSPACE => c"Buffer too small",
        ESPANK_NOT_REMOTE => c"Valid only in remote context",
        ESPANK_NOEXIST => c"Id/pid does not exist on this node",
        ESPANK_NOT_AVAIL => c"Item not available from this callback",
        ESPANK_NOT_LOCAL => c"Valid only in local or allocator context",
        _ => c"Generic error",
    };
    s.as_ptr()
}

/// Copy `map[key]` into the C buffer `buf` of size `len`, NUL-terminating.
///
/// Returns `ESPANK_ENV_NOEXIST` if the key is absent, or `ESPANK_NOSPACE` if
/// the value plus its NUL would not fit (in which case nothing is written).
fn copy_env_value(map: &HashMap<String, String>, key: &str, buf: *mut c_char, len: c_int) -> c_int {
    let Some(value) = map.get(key) else {
        return ESPANK_ENV_NOEXIST;
    };
    let bytes = value.as_bytes();
    let cap = len as usize;
    // Reserve one byte for the trailing NUL.
    if bytes.len() + 1 > cap {
        return ESPANK_NOSPACE;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, bytes.len());
        *buf.add(bytes.len()) = 0;
    }
    ESPANK_SUCCESS
}

/// Slurm logging functions exported to plugins.
///
/// Slurm declares these variadic (`printf`-style). Stable Rust cannot define
/// C-variadic functions, so these take only the format string and log it
/// verbatim via `tracing`. This keeps symbols resolvable and is ABI-safe on
/// the SysV varargs convention (extra args are ignored); the trade-off is that
/// `%`-style format specifiers are not expanded.
macro_rules! spank_log_fn {
    ($name:ident, $level:ident) => {
        #[no_mangle]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn $name(fmt: *const c_char) {
            if fmt.is_null() {
                return;
            }
            let msg = unsafe { CStr::from_ptr(fmt) }.to_string_lossy();
            $level!(target: "spank_plugin", "{msg}");
        }
    };
}

spank_log_fn!(slurm_error, error);
spank_log_fn!(slurm_info, info);
spank_log_fn!(slurm_verbose, info);
spank_log_fn!(slurm_debug, debug);
spank_log_fn!(slurm_debug2, debug);
spank_log_fn!(slurm_debug3, debug);
spank_log_fn!(slurm_spank_log, info);

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
        let mut handle = SpankHandle::new(SpankContext::default(), HashMap::new());
        // Invoking hooks on empty host should succeed (no plugins to fail)
        assert!(host.invoke_hook(SpankHook::Init, &mut handle).is_ok());
        assert!(host.invoke_hook(SpankHook::TaskExit, &mut handle).is_ok());
        assert!(host.invoke_hook(SpankHook::JobEpilog, &mut handle).is_ok());
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
        writeln!(f).unwrap();
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
    fn test_spank_item_ids_match_slurm() {
        // Discriminants must equal Slurm's enum spank_item values.
        assert_eq!(SpankItem::JobUid as c_int, 0);
        assert_eq!(SpankItem::JobGid as c_int, 1);
        assert_eq!(SpankItem::JobId as c_int, 2);
        assert_eq!(SpankItem::JobStepId as c_int, 3);
        assert_eq!(SpankItem::JobNnodes as c_int, 4);
        assert_eq!(SpankItem::JobNodeid as c_int, 5);
        assert_eq!(SpankItem::JobLocalTaskCount as c_int, 6);
        assert_eq!(SpankItem::JobTotalTaskCount as c_int, 7);
        assert_eq!(SpankItem::JobNcpus as c_int, 8);
        assert_eq!(SpankItem::JobArgv as c_int, 9);
        assert_eq!(SpankItem::TaskPid as c_int, 14);
    }

    fn test_handle() -> SpankHandle {
        SpankHandle {
            context: SpankContext {
                job_id: 42,
                uid: 1000,
                gid: 1001,
                step_id: 3,
                num_nodes: 2,
                node_id: 1,
                local_task_count: 4,
                total_task_count: 8,
                task_pid: 12345,
            },
            env: HashMap::new(),
            job_control_env: HashMap::new(),
        }
    }

    fn get_item_u32(handle: &mut SpankHandle, item: SpankItem) -> Option<u32> {
        let mut out: u32 = 0;
        let rc = spank_get_item(
            handle,
            item as c_int,
            &mut out as *mut u32 as *mut std::ffi::c_void,
        );
        (rc == ESPANK_SUCCESS).then_some(out)
    }

    #[test]
    fn test_spank_get_item_returns_correct_fields() {
        let mut handle = test_handle();
        assert_eq!(get_item_u32(&mut handle, SpankItem::JobId), Some(42));
        assert_eq!(get_item_u32(&mut handle, SpankItem::JobUid), Some(1000));
        assert_eq!(get_item_u32(&mut handle, SpankItem::JobGid), Some(1001));
        assert_eq!(get_item_u32(&mut handle, SpankItem::TaskPid), Some(12345));
    }

    #[test]
    fn test_spank_get_item_not_available_vs_bad_arg() {
        let mut handle = test_handle();
        let mut out: u32 = 0;
        let out_ptr = &mut out as *mut u32 as *mut std::ffi::c_void;
        assert_eq!(
            spank_get_item(&mut handle, SpankItem::JobNcpus as c_int, out_ptr),
            ESPANK_NOT_AVAIL
        );
        assert_eq!(spank_get_item(&mut handle, 9999, out_ptr), ESPANK_BAD_ARG);
    }

    #[test]
    fn test_spank_setenv_overwrite_semantics() {
        let mut handle = test_handle();
        let var = std::ffi::CString::new("FOO").unwrap();
        let val1 = std::ffi::CString::new("one").unwrap();
        let val2 = std::ffi::CString::new("two").unwrap();

        assert_eq!(
            spank_setenv(&mut handle, var.as_ptr(), val1.as_ptr(), 0),
            ESPANK_SUCCESS
        );
        // overwrite = 0 on existing key must not clobber.
        assert_eq!(
            spank_setenv(&mut handle, var.as_ptr(), val2.as_ptr(), 0),
            ESPANK_ENV_EXISTS
        );
        assert_eq!(handle.env.get("FOO").map(String::as_str), Some("one"));
        // overwrite = 1 replaces it.
        assert_eq!(
            spank_setenv(&mut handle, var.as_ptr(), val2.as_ptr(), 1),
            ESPANK_SUCCESS
        );
        assert_eq!(handle.env.get("FOO").map(String::as_str), Some("two"));
    }

    #[test]
    fn test_spank_getenv_roundtrip_and_errors() {
        let mut handle = test_handle();
        let var = std::ffi::CString::new("BAR").unwrap();
        let val = std::ffi::CString::new("baz").unwrap();
        spank_setenv(&mut handle, var.as_ptr(), val.as_ptr(), 1);

        let mut buf = [0i8; 16];
        assert_eq!(
            spank_getenv(
                &mut handle,
                var.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as c_int
            ),
            ESPANK_SUCCESS
        );
        let out = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        assert_eq!(out, "baz");

        // Missing key.
        let missing = std::ffi::CString::new("NOPE").unwrap();
        assert_eq!(
            spank_getenv(
                &mut handle,
                missing.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as c_int
            ),
            ESPANK_ENV_NOEXIST
        );

        // Buffer too small (value "baz" needs 4 bytes incl. NUL).
        let mut tiny = [0i8; 2];
        assert_eq!(
            spank_getenv(
                &mut handle,
                var.as_ptr(),
                tiny.as_mut_ptr(),
                tiny.len() as c_int
            ),
            ESPANK_NOSPACE
        );
    }

    #[test]
    fn test_spank_job_control_setenv_prefixes_key() {
        let mut handle = test_handle();
        let name = std::ffi::CString::new("TOKEN").unwrap();
        let val = std::ffi::CString::new("secret").unwrap();
        assert_eq!(
            spank_job_control_setenv(&mut handle, name.as_ptr(), val.as_ptr(), 0),
            ESPANK_SUCCESS
        );
        assert_eq!(
            handle
                .job_control_env
                .get("SPANK_TOKEN")
                .map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn test_spank_strerror_success() {
        let ptr = spank_strerror(ESPANK_SUCCESS);
        let s = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap();
        assert_eq!(s, "Success");
    }

    #[test]
    fn test_spank_handle_new_seeds_context_and_env() {
        let mut env = HashMap::new();
        env.insert("A".to_string(), "1".to_string());
        let ctx = SpankContext {
            job_id: 7,
            ..Default::default()
        };
        let handle = SpankHandle::new(ctx, env);
        assert_eq!(handle.context.job_id, 7);
        assert_eq!(handle.env.get("A").map(String::as_str), Some("1"));
        assert!(handle.job_control_env.is_empty());
    }
}
