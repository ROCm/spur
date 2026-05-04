//! C FFI compatibility shim — libslurm-compat.so
//!
//! Exports extern "C" functions matching a subset of Slurm's libslurm ABI,
//! allowing existing tools that link against libslurm to use Spur instead.

mod types;

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::OnceLock;

use types::*;

/// Global controller address (set via SLURM_CONTROLLER_ADDR env var or slurm_init).
static CONTROLLER_ADDR: OnceLock<String> = OnceLock::new();

fn controller_addr() -> &'static str {
    CONTROLLER_ADDR.get_or_init(|| {
        std::env::var("SLURM_CONTROLLER_ADDR").unwrap_or_else(|_| "http://localhost:6817".into())
    })
}

/// Global tokio runtime for async operations.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("failed to create tokio runtime"))
}

// ============================================================
// Core FFI Functions (MVP: 6 functions)
// ============================================================

/// Initialize a job descriptor to defaults.
///
/// Equivalent to slurm_init_job_desc_msg().
#[no_mangle]
pub extern "C" fn slurm_init_job_desc_msg(desc: *mut SlurmJobDescMsg) {
    if desc.is_null() {
        return;
    }
    unsafe {
        let d = &mut *desc;
        *d = SlurmJobDescMsg::default();
    }
}

/// Submit a batch job.
///
/// Returns 0 on success, -1 on error.
/// On success, *job_id is set to the assigned job ID.
#[no_mangle]
pub extern "C" fn slurm_submit_batch_job(
    desc: *const SlurmJobDescMsg,
    job_id: *mut c_uint,
) -> c_int {
    if desc.is_null() || job_id.is_null() {
        return -1;
    }

    let desc = unsafe { &*desc };
    let result = runtime().block_on(async {
        use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
        use spur_proto::proto::{JobSpec, SubmitJobRequest};

        let mut client = SlurmControllerClient::connect(controller_addr().to_string())
            .await
            .ok()?;

        let spec = JobSpec {
            name: c_str_to_string(desc.name),
            partition: c_str_to_string(desc.partition),
            account: c_str_to_string(desc.account),
            script: c_str_to_string(desc.script),
            work_dir: c_str_to_string(desc.work_dir),
            num_nodes: desc.min_nodes,
            num_tasks: desc.num_tasks,
            cpus_per_task: desc.cpus_per_task,
            time_limit: if desc.time_limit > 0 {
                Some(prost_types::Duration {
                    seconds: desc.time_limit as i64 * 60,
                    nanos: 0,
                })
            } else {
                None
            },
            ..Default::default()
        };

        let resp = client
            .submit_job(SubmitJobRequest { spec: Some(spec) })
            .await
            .ok()?;

        Some(resp.into_inner().job_id)
    });

    match result {
        Some(id) => {
            unsafe { *job_id = id };
            0
        }
        None => -1,
    }
}

/// Load job information for all jobs or specific job IDs.
///
/// Caller must free with slurm_free_job_info_msg().
#[no_mangle]
pub extern "C" fn slurm_load_jobs(
    _update_time: i64,
    job_info_msg: *mut *mut SlurmJobInfoMsg,
    _show_flags: c_uint,
) -> c_int {
    if job_info_msg.is_null() {
        return -1;
    }

    let result = runtime().block_on(async {
        use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
        use spur_proto::proto::GetJobsRequest;

        let mut client = SlurmControllerClient::connect(controller_addr().to_string())
            .await
            .ok()?;

        let resp = client.get_jobs(GetJobsRequest::default()).await.ok()?;

        Some(resp.into_inner().jobs)
    });

    match result {
        Some(jobs) => {
            let count = jobs.len();
            let mut c_jobs: Vec<SlurmJobInfo> = jobs
                .iter()
                .map(|j| SlurmJobInfo {
                    job_id: j.job_id,
                    name: string_to_c_str(&j.name),
                    user_name: string_to_c_str(&j.user),
                    partition: string_to_c_str(&j.partition),
                    account: string_to_c_str(&j.account),
                    job_state: j.state as u32,
                    num_nodes: j.num_nodes,
                    num_tasks: j.num_tasks,
                    exit_code: j.exit_code,
                    nodelist: string_to_c_str(&j.nodelist),
                })
                .collect();

            let msg = Box::new(SlurmJobInfoMsg {
                record_count: count as u32,
                job_array: c_jobs.as_mut_ptr(),
                _job_storage: c_jobs,
            });

            unsafe {
                *job_info_msg = Box::into_raw(msg);
            }
            0
        }
        None => -1,
    }
}

/// Free a job info message.
#[no_mangle]
pub extern "C" fn slurm_free_job_info_msg(msg: *mut SlurmJobInfoMsg) {
    if !msg.is_null() {
        unsafe {
            let msg = Box::from_raw(msg);
            // Free C strings in job records
            for job in &msg._job_storage {
                free_c_str(job.name);
                free_c_str(job.user_name);
                free_c_str(job.partition);
                free_c_str(job.account);
                free_c_str(job.nodelist);
            }
        }
    }
}

/// Load node information.
#[no_mangle]
pub extern "C" fn slurm_load_node(
    _update_time: i64,
    node_info_msg: *mut *mut SlurmNodeInfoMsg,
    _show_flags: c_uint,
) -> c_int {
    if node_info_msg.is_null() {
        return -1;
    }

    let result = runtime().block_on(async {
        use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
        use spur_proto::proto::GetNodesRequest;

        let mut client = SlurmControllerClient::connect(controller_addr().to_string())
            .await
            .ok()?;

        let resp = client.get_nodes(GetNodesRequest::default()).await.ok()?;

        Some(resp.into_inner().nodes)
    });

    match result {
        Some(nodes) => {
            let count = nodes.len();
            let mut c_nodes: Vec<SlurmNodeInfo> = nodes
                .iter()
                .map(|n| SlurmNodeInfo {
                    name: string_to_c_str(&n.name),
                    node_state: n.state as u32,
                    cpus: n.total_resources.as_ref().map(|r| r.cpus).unwrap_or(0),
                    real_memory: n.total_resources.as_ref().map(|r| r.memory_mb).unwrap_or(0),
                    reason: string_to_c_str(&n.state_reason),
                })
                .collect();

            let msg = Box::new(SlurmNodeInfoMsg {
                record_count: count as u32,
                node_array: c_nodes.as_mut_ptr(),
                _node_storage: c_nodes,
            });

            unsafe {
                *node_info_msg = Box::into_raw(msg);
            }
            0
        }
        None => -1,
    }
}

/// Load partition information.
#[no_mangle]
pub extern "C" fn slurm_load_partitions(
    _update_time: i64,
    part_info_msg: *mut *mut SlurmPartInfoMsg,
    _show_flags: c_uint,
) -> c_int {
    if part_info_msg.is_null() {
        return -1;
    }

    let result = runtime().block_on(async {
        use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
        use spur_proto::proto::GetPartitionsRequest;

        let mut client = SlurmControllerClient::connect(controller_addr().to_string())
            .await
            .ok()?;

        let resp = client
            .get_partitions(GetPartitionsRequest::default())
            .await
            .ok()?;

        Some(resp.into_inner().partitions)
    });

    match result {
        Some(parts) => {
            let count = parts.len();
            let mut c_parts: Vec<SlurmPartInfo> = parts
                .iter()
                .map(|p| SlurmPartInfo {
                    name: string_to_c_str(&p.name),
                    total_nodes: p.total_nodes,
                    total_cpus: p.total_cpus,
                    nodes: string_to_c_str(&p.nodes),
                })
                .collect();

            let msg = Box::new(SlurmPartInfoMsg {
                record_count: count as u32,
                partition_array: c_parts.as_mut_ptr(),
                _part_storage: c_parts,
            });

            unsafe {
                *part_info_msg = Box::into_raw(msg);
            }
            0
        }
        None => -1,
    }
}

/// Kill (cancel) a job.
#[no_mangle]
pub extern "C" fn slurm_kill_job(job_id: c_uint, signal: u16, _flags: u16) -> c_int {
    let result = runtime().block_on(async {
        use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
        use spur_proto::proto::CancelJobRequest;

        let mut client = SlurmControllerClient::connect(controller_addr().to_string())
            .await
            .ok()?;

        client
            .cancel_job(CancelJobRequest {
                job_id,
                signal: signal as i32,
                user: String::new(),
            })
            .await
            .ok()?;

        Some(())
    });

    match result {
        Some(()) => 0,
        None => -1,
    }
}

// ============================================================
// Error Handling
// ============================================================

use std::sync::Mutex;

static LAST_ERRNO: Mutex<c_int> = Mutex::new(0);

static ERROR_STRINGS: &[&str] = &[
    "Success",                           // 0
    "Unspecified error",                 // -1
    "Invalid job id",                    // -2
    "Invalid job id specified",          // -3
    "Invalid node name specified",       // -4
    "Invalid partition name specified",  // -5
    "Job already completed",             // -6
    "Job already completing",            // -7
    "Communication failure",             // -8
    "Out of memory",                     // -9
    "Permission denied",                 // -10
    "Node not available",                // -11
    "Already done",                      // -12
    "Requested node config unavailable", // -13
    "Job pending",                       // -14
];

/// Get a human-readable error string for a Slurm error code.
#[no_mangle]
pub extern "C" fn slurm_strerror(errnum: c_int) -> *const c_char {
    let idx = if errnum == 0 {
        0
    } else if errnum < 0 && (-errnum as usize) < ERROR_STRINGS.len() {
        (-errnum) as usize
    } else {
        1 // "Unspecified error"
    };
    // These are static strings, safe to return as pointers
    ERROR_STRINGS[idx].as_ptr() as *const c_char
}

/// Get the last Slurm error code.
#[no_mangle]
pub extern "C" fn slurm_get_errno() -> c_int {
    *LAST_ERRNO.lock().unwrap_or_else(|e| e.into_inner())
}

/// Set the Slurm error code.
#[no_mangle]
pub extern "C" fn slurm_seterrno(errnum: c_int) {
    *LAST_ERRNO.lock().unwrap_or_else(|e| e.into_inner()) = errnum;
}

/// Print a Slurm error message to stderr.
#[no_mangle]
pub extern "C" fn slurm_perror(msg: *const c_char) {
    let prefix = c_str_to_string(msg);
    let errno = slurm_get_errno();
    let idx = if errno == 0 {
        0
    } else if errno < 0 && (-errno as usize) < ERROR_STRINGS.len() {
        (-errno) as usize
    } else {
        1
    };
    eprintln!("{}: {}", prefix, ERROR_STRINGS[idx]);
}

// ============================================================
// Helpers
// ============================================================

fn c_str_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

fn string_to_c_str(s: &str) -> *mut c_char {
    CString::new(s)
        .unwrap_or_else(|_| CString::new("").unwrap())
        .into_raw()
}

fn free_c_str(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}
