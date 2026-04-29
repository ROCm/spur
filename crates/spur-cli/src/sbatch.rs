use anyhow::{bail, Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{JobSpec, SubmitJobRequest};
use std::collections::HashMap;

/// Submit a batch job script.
#[derive(Parser, Debug)]
#[command(name = "sbatch", about = "Submit a batch job script")]
pub struct SbatchArgs {
    // CLI args override #SBATCH directives. clap merges directives + CLI argv;
    // `overrides_with = "self"` allows duplicates with last-wins, so a CLI flag
    // appearing after a directive flag wins. Vec args (gres, licenses,
    // container_mounts, container_env) accumulate by design and have no
    // `overrides_with`. Closes #143.
    /// Job name
    #[arg(short = 'J', long, overrides_with = "job_name")]
    pub job_name: Option<String>,

    /// Partition
    #[arg(short = 'p', long, overrides_with = "partition")]
    pub partition: Option<String>,

    /// Account
    #[arg(short = 'A', long, overrides_with = "account")]
    pub account: Option<String>,

    /// Number of nodes
    #[arg(short = 'N', long, default_value = "1", overrides_with = "nodes")]
    pub nodes: u32,

    /// Number of tasks
    #[arg(short = 'n', long, default_value = "1", overrides_with = "ntasks")]
    pub ntasks: u32,

    /// Tasks per node
    #[arg(long, overrides_with = "ntasks_per_node")]
    pub ntasks_per_node: Option<u32>,

    /// CPUs per task
    #[arg(
        short = 'c',
        long,
        default_value = "1",
        overrides_with = "cpus_per_task"
    )]
    pub cpus_per_task: u32,

    /// Memory per node (e.g., "4G", "4096M", "4096")
    #[arg(long, overrides_with = "mem")]
    pub mem: Option<String>,

    /// Memory per CPU
    #[arg(long, overrides_with = "mem_per_cpu")]
    pub mem_per_cpu: Option<String>,

    /// Generic resources (e.g., "gpu:4", "gpu:mi300x:8")
    #[arg(long)]
    pub gres: Vec<String>,

    /// Licenses (e.g., "fluent:5", "matlab:1")
    #[arg(short = 'L', long)]
    pub licenses: Vec<String>,

    /// GPUs (shorthand, e.g., "4" or "mi300x:4")
    #[arg(short = 'G', long, overrides_with = "gpus")]
    pub gpus: Option<String>,

    /// GPUs per node
    #[arg(long, overrides_with = "gpus_per_node")]
    pub gpus_per_node: Option<String>,

    /// Time limit (e.g., "4:00:00", "1-00:00:00")
    #[arg(short = 't', long, overrides_with = "time")]
    pub time: Option<String>,

    /// Minimum time limit
    #[arg(long, overrides_with = "time_min")]
    pub time_min: Option<String>,

    /// Working directory
    #[arg(short = 'D', long, overrides_with = "chdir")]
    pub chdir: Option<String>,

    /// Stdout file
    #[arg(short = 'o', long, overrides_with = "output")]
    pub output: Option<String>,

    /// Stderr file
    #[arg(short = 'e', long, overrides_with = "error")]
    pub error: Option<String>,

    /// QoS
    #[arg(short = 'q', long, overrides_with = "qos")]
    pub qos: Option<String>,

    /// Job dependency (e.g., "afterok:123")
    #[arg(short = 'd', long, overrides_with = "dependency")]
    pub dependency: Option<String>,

    /// Node list
    #[arg(short = 'w', long, overrides_with = "nodelist")]
    pub nodelist: Option<String>,

    /// Exclude nodes
    #[arg(short = 'x', long, overrides_with = "exclude")]
    pub exclude: Option<String>,

    /// Required node features (e.g., "mi300x,nvlink")
    #[arg(short = 'C', long, overrides_with = "constraint")]
    pub constraint: Option<String>,

    /// Target a named reservation
    #[arg(long, overrides_with = "reservation")]
    pub reservation: Option<String>,

    /// Job array (e.g., "0-99%10")
    #[arg(short = 'a', long, overrides_with = "array")]
    pub array: Option<String>,

    /// Task distribution (block, cyclic, plane, arbitrary)
    #[arg(short = 'm', long, overrides_with = "distribution")]
    pub distribution: Option<String>,

    /// Heterogeneous job component index (0 = first component)
    #[arg(long, overrides_with = "het_group")]
    pub het_group: Option<u32>,

    /// Burst buffer specification ("stage_in:cmd;stage_out:cmd")
    #[arg(long, overrides_with = "bb")]
    pub bb: Option<String>,

    /// Earliest start time (ISO 8601, e.g. "2026-03-22T10:00:00Z" or "now+1hour")
    #[arg(long, overrides_with = "begin")]
    pub begin: Option<String>,

    /// Cancel if still pending after this time (ISO 8601)
    #[arg(long, overrides_with = "deadline")]
    pub deadline: Option<String>,

    /// Spread job across least-loaded nodes
    #[arg(long, overrides_with = "spread_job")]
    pub spread_job: bool,

    /// Topology-aware scheduling: "tree" (minimize switch hops) or "block" (keep within rack)
    #[arg(long, overrides_with = "topology")]
    pub topology: Option<String>,

    /// Output file open mode: "truncate" (default) or "append"
    #[arg(long, overrides_with = "open_mode")]
    pub open_mode: Option<String>,

    /// MPI type (none, pmix, pmi2)
    #[arg(long, default_value = "none", overrides_with = "mpi")]
    pub mpi: String,

    /// Allow requeue
    #[arg(long, overrides_with = "requeue")]
    pub requeue: bool,

    /// Exclusive node access
    #[arg(long, overrides_with = "exclusive")]
    pub exclusive: bool,

    /// Hold job
    #[arg(short = 'H', long, overrides_with = "hold")]
    pub hold: bool,

    /// Comment
    #[arg(long, overrides_with = "comment")]
    pub comment: Option<String>,

    /// Mail notification type (comma-separated: BEGIN,END,FAIL,ALL)
    #[arg(long, overrides_with = "mail_type")]
    pub mail_type: Option<String>,

    /// Mail notification user/address
    #[arg(long, overrides_with = "mail_user")]
    pub mail_user: Option<String>,

    /// Export environment variables
    #[arg(long, default_value = "ALL", overrides_with = "export")]
    pub export: String,

    // Container
    /// Container image (OCI ref or squashfs path)
    #[arg(long, overrides_with = "container_image")]
    pub container_image: Option<String>,

    /// Container bind mounts ("/src:/dst:ro")
    #[arg(long)]
    pub container_mounts: Vec<String>,

    /// Working directory inside the container
    #[arg(long, overrides_with = "container_workdir")]
    pub container_workdir: Option<String>,

    /// Named container (persists across jobs)
    #[arg(long, overrides_with = "container_name")]
    pub container_name: Option<String>,

    /// Read-only container rootfs
    #[arg(long, overrides_with = "container_readonly")]
    pub container_readonly: bool,

    /// Mount user home directory in container
    #[arg(long, overrides_with = "container_mount_home")]
    pub container_mount_home: bool,

    /// Set environment variable inside container (KEY=VAL)
    #[arg(long)]
    pub container_env: Vec<String>,

    /// Override container entrypoint
    #[arg(long, overrides_with = "container_entrypoint")]
    pub container_entrypoint: Option<String>,

    /// Remap user to root inside container
    #[arg(long, overrides_with = "container_remap_root")]
    pub container_remap_root: bool,

    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817",
        overrides_with = "controller"
    )]
    pub controller: String,

    /// The batch script file
    pub script: Option<String>,
}

/// Parse #SBATCH directives from a script, returning them as argv-style strings.
pub fn parse_sbatch_directives(script: &str) -> Vec<String> {
    let mut args = Vec::new();
    for line in script.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#SBATCH") {
            let rest = rest.trim();
            if !rest.is_empty() {
                // Split on whitespace, preserving quoted strings
                args.extend(shell_split(rest));
            }
        }
        // Also parse #PBS for migration
        if let Some(rest) = trimmed.strip_prefix("#PBS") {
            let rest = rest.trim();
            if !rest.is_empty() {
                if let Some(converted) = convert_pbs_to_sbatch(rest) {
                    args.extend(shell_split(&converted));
                }
            }
        }
        // Stop at first non-comment, non-blank, non-shebang line
        if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with("#!/") {
            break;
        }
    }
    args
}

/// Build the argv that clap parses: directives first, CLI args after.
///
/// Order is load-bearing: scalar args in `SbatchArgs` use `overrides_with =
/// "self"` for last-wins semantics, so a CLI flag appearing after a directive
/// flag wins. Vec args (e.g. `--gres`) accumulate from both sources.
pub fn merge_directives_and_cli(directives: &[String], cli_args: &[String]) -> Vec<String> {
    let mut merged = vec!["sbatch".to_string()];
    merged.extend(directives.iter().cloned());
    merged.extend(cli_args.iter().skip(1).cloned()); // skip argv[0]
    merged
}

/// Basic shell word splitting (handles simple quoting).
fn shell_split(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for c in s.chars() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Convert a PBS directive to sbatch equivalent (best-effort).
fn convert_pbs_to_sbatch(pbs_arg: &str) -> Option<String> {
    let parts: Vec<&str> = pbs_arg.splitn(2, ' ').collect();
    let flag = parts[0].trim_start_matches('-');
    let value = parts.get(1).map(|s| s.trim());

    match flag {
        "N" => value.map(|v| format!("--job-name={}", v)),
        "q" => value.map(|v| format!("--partition={}", v)),
        "l" => {
            // PBS resource specs like "walltime=4:00:00" or "nodes=2:ppn=8"
            value.and_then(|v| convert_pbs_resource(v))
        }
        "o" => value.map(|v| format!("--output={}", v)),
        "e" => value.map(|v| format!("--error={}", v)),
        "A" => value.map(|v| format!("--account={}", v)),
        _ => None,
    }
}

fn convert_pbs_resource(spec: &str) -> Option<String> {
    for part in spec.split(',') {
        let kv: Vec<&str> = part.splitn(2, '=').collect();
        if kv.len() == 2 {
            match kv[0] {
                "walltime" => return Some(format!("--time={}", kv[1])),
                "nodes" => {
                    // "nodes=2:ppn=8" → "--nodes=2 --ntasks-per-node=8"
                    let node_parts: Vec<&str> = kv[1].split(':').collect();
                    let mut result = format!("--nodes={}", node_parts[0]);
                    for np in &node_parts[1..] {
                        if let Some(ppn) = np.strip_prefix("ppn=") {
                            result.push_str(&format!(" --ntasks-per-node={}", ppn));
                        }
                    }
                    return Some(result);
                }
                "mem" => return Some(format!("--mem={}", kv[1])),
                _ => {}
            }
        }
    }
    None
}

/// Parse a datetime argument. Supports:
///   - ISO 8601: "2026-03-22T10:00:00Z"
///   - "now" → current time
///   - "now+Nhours", "now+Nminutes" → offset from now
fn parse_datetime_arg(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("now") {
        return Ok(chrono::Utc::now());
    }
    if let Some(rest) = s.strip_prefix("now+").or_else(|| s.strip_prefix("now +")) {
        let rest = rest.trim();
        // Try "Nhours", "Nminutes", "Nseconds", "Ndays"
        if let Some(n) = rest
            .strip_suffix("hours")
            .or_else(|| rest.strip_suffix("hour"))
        {
            let n: i64 = n.trim().parse().context("invalid offset")?;
            return Ok(chrono::Utc::now() + chrono::Duration::hours(n));
        }
        if let Some(n) = rest
            .strip_suffix("minutes")
            .or_else(|| rest.strip_suffix("minute"))
        {
            let n: i64 = n.trim().parse().context("invalid offset")?;
            return Ok(chrono::Utc::now() + chrono::Duration::minutes(n));
        }
        if let Some(n) = rest
            .strip_suffix("seconds")
            .or_else(|| rest.strip_suffix("second"))
        {
            let n: i64 = n.trim().parse().context("invalid offset")?;
            return Ok(chrono::Utc::now() + chrono::Duration::seconds(n));
        }
        if let Some(n) = rest
            .strip_suffix("days")
            .or_else(|| rest.strip_suffix("day"))
        {
            let n: i64 = n.trim().parse().context("invalid offset")?;
            return Ok(chrono::Utc::now() + chrono::Duration::days(n));
        }
        bail!("unsupported time offset format: {}", rest);
    }
    // Try ISO 8601 parse
    s.parse::<chrono::DateTime<chrono::Utc>>()
        .context("invalid datetime format (expected ISO 8601 or 'now+Nhours')")
}

/// Parse memory string (e.g., "4G", "4096M", "4096") into MB.
fn parse_memory_mb(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Some(gb) = s.strip_suffix('G').or_else(|| s.strip_suffix('g')) {
        let val: f64 = gb.parse().context("invalid memory value")?;
        Ok((val * 1024.0) as u64)
    } else if let Some(mb) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        Ok(mb.parse().context("invalid memory value")?)
    } else if let Some(kb) = s.strip_suffix('K').or_else(|| s.strip_suffix('k')) {
        let val: u64 = kb.parse().context("invalid memory value")?;
        Ok(val / 1024)
    } else {
        // Default: MB
        Ok(s.parse().context("invalid memory value")?)
    }
}

/// Resolve a container image name to an absolute squashfs path if possible.
///
/// When an image is imported via `spur image import`, it is stored in the
/// local image directory (e.g., `/var/spool/spur/images` or `~/.spur/images`).
/// By resolving to an absolute path at submit time, compute node agents can
/// find the image directly — this works when the login node and compute nodes
/// share a filesystem (NFS, Lustre, etc.).
///
/// If the image is already an absolute path, or if the local `.sqsh` file
/// cannot be found, the original value is returned unchanged so the agent
/// can attempt its own resolution.
fn resolve_container_image(image: Option<&str>) -> String {
    let image = match image {
        Some(s) if !s.is_empty() => s,
        _ => return String::new(),
    };

    // If already an absolute path, keep as-is
    if image.starts_with('/') {
        return image.to_string();
    }

    // Try to find the .sqsh file in the image directory
    let sanitized = spur_net::oci::sanitize_name(image);

    // Check $SPUR_IMAGE_DIR first, then system default, then user fallback
    let candidates = {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("SPUR_IMAGE_DIR") {
            if !dir.is_empty() {
                dirs.push(std::path::PathBuf::from(dir));
            }
        }
        dirs.push(std::path::PathBuf::from("/var/spool/spur/images"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(std::path::PathBuf::from(home).join(".spur/images"));
        }
        dirs
    };

    for dir in candidates {
        let path = dir.join(format!("{}.sqsh", sanitized));
        if path.exists() {
            return path.to_string_lossy().into_owned();
        }
    }

    // Not found locally — return original name so agent can try its own lookup
    image.to_string()
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(cli_args: Vec<String>) -> Result<()> {
    // If script is provided, parse directives from it
    let script_content = if let Some(script_path) = cli_args.last() {
        if !script_path.starts_with('-') && script_path != "sbatch" {
            match std::fs::read_to_string(script_path) {
                Ok(content) => Some(content),
                Err(_) => None,
            }
        } else {
            None
        }
    } else {
        None
    };

    let directive_args = script_content
        .as_deref()
        .map(parse_sbatch_directives)
        .unwrap_or_default();
    let merged_args = merge_directives_and_cli(&directive_args, &cli_args);

    let args = SbatchArgs::try_parse_from(&merged_args)?;

    // Build the job spec
    let script = match &args.script {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read script: {}", path))?,
        None => {
            if atty::is(atty::Stream::Stdin) {
                bail!("sbatch: no script file specified");
            }
            // Read from stdin
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf
        }
    };

    let work_dir = args
        .chdir
        .unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().into());

    let name = args
        .job_name
        .unwrap_or_else(|| args.script.as_deref().unwrap_or("sbatch").to_string());

    // Build GRES list
    let mut gres = args.gres;
    if let Some(gpus) = &args.gpus {
        gres.push(format!("gpu:{}", gpus));
    }
    if let Some(gpn) = &args.gpus_per_node {
        gres.push(format!("gpu:{}", gpn));
    }
    // Append licenses as GRES entries (license:<name>:<count>)
    for lic in &args.licenses {
        gres.push(format!("license:{}", lic));
    }

    // Parse time limit — use parse_time_seconds so that short values like
    // "0:00:10" (10 seconds) are stored with full second precision instead of
    // being rounded up to the nearest minute.
    let time_limit = args
        .time
        .as_ref()
        .and_then(|t| spur_core::config::parse_time_seconds(t))
        .map(|secs| prost_types::Duration {
            seconds: secs as i64,
            nanos: 0,
        });

    // Parse memory
    let memory_per_node = args.mem.as_ref().map(|m| parse_memory_mb(m)).transpose()?;
    let memory_per_cpu = args
        .mem_per_cpu
        .as_ref()
        .map(|m| parse_memory_mb(m))
        .transpose()?;

    // Build environment
    let environment: HashMap<String, String> = if args.export == "ALL" {
        std::env::vars().collect()
    } else if args.export == "NONE" {
        HashMap::new()
    } else {
        std::env::vars()
            .filter(|(k, _)| args.export.split(',').any(|e| e == k))
            .collect()
    };

    // Parse dependencies
    let dependencies: Vec<String> = args
        .dependency
        .map(|d| d.split(',').map(String::from).collect())
        .unwrap_or_default();

    let job_spec = JobSpec {
        name,
        partition: args.partition.unwrap_or_default(),
        account: args.account.unwrap_or_default(),
        user: whoami::username().unwrap_or_else(|_| "unknown".into()),
        uid: nix::unistd::getuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        num_nodes: args.nodes,
        num_tasks: args.ntasks,
        tasks_per_node: args.ntasks_per_node.unwrap_or(0),
        cpus_per_task: args.cpus_per_task,
        memory_per_node_mb: memory_per_node.unwrap_or(0),
        memory_per_cpu_mb: memory_per_cpu.unwrap_or(0),
        gres,
        script,
        argv: Vec::new(),
        work_dir,
        stdout_path: args.output.unwrap_or_default(),
        stderr_path: args.error.unwrap_or_default(),
        environment,
        time_limit,
        time_min: None,
        qos: args.qos.unwrap_or_default(),
        priority: 0,
        reservation: args.reservation.unwrap_or_default(),
        dependency: dependencies,
        nodelist: args.nodelist.unwrap_or_default(),
        exclude: args.exclude.unwrap_or_default(),
        constraint: args.constraint.unwrap_or_default(),
        mpi: args.mpi,
        distribution: args.distribution.unwrap_or_default(),
        het_group: args.het_group.unwrap_or(0),
        array_spec: args.array.unwrap_or_default(),
        requeue: args.requeue,
        exclusive: args.exclusive,
        hold: args.hold,
        comment: args.comment.unwrap_or_default(),
        wckey: String::new(),
        container_image: resolve_container_image(args.container_image.as_deref()),
        container_mounts: args.container_mounts,
        container_workdir: args.container_workdir.unwrap_or_default(),
        container_name: args.container_name.unwrap_or_default(),
        container_readonly: args.container_readonly,
        container_mount_home: args.container_mount_home,
        container_env: args
            .container_env
            .iter()
            .filter_map(|s| {
                s.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect(),
        container_entrypoint: args.container_entrypoint.unwrap_or_default(),
        container_remap_root: args.container_remap_root,
        burst_buffer: args.bb.unwrap_or_default(),
        licenses: args.licenses,
        mail_type: args
            .mail_type
            .map(|s| s.split(',').map(|t| t.trim().to_uppercase()).collect())
            .unwrap_or_default(),
        mail_user: args.mail_user.unwrap_or_default(),
        interactive: false,
        begin_time: args
            .begin
            .as_ref()
            .map(|s| parse_datetime_arg(s))
            .transpose()?
            .map(|dt| prost_types::Timestamp {
                seconds: dt.timestamp(),
                nanos: dt.timestamp_subsec_nanos() as i32,
            }),
        deadline: args
            .deadline
            .as_ref()
            .map(|s| parse_datetime_arg(s))
            .transpose()?
            .map(|dt| prost_types::Timestamp {
                seconds: dt.timestamp(),
                nanos: dt.timestamp_subsec_nanos() as i32,
            }),
        spread_job: args.spread_job,
        topology: args.topology.clone().unwrap_or_default(),
        host_network: false,
        privileged: false,
        host_ipc: false,
        shm_size: String::new(),
        extra_resources: std::collections::HashMap::new(),
        open_mode: args.open_mode.unwrap_or_default(),
    };

    // Submit to controller
    let mut client = SlurmControllerClient::connect(args.controller)
        .await
        .context("failed to connect to spurctld")?;

    let response = client
        .submit_job(SubmitJobRequest {
            spec: Some(job_spec),
        })
        .await
        .context("job submission failed")?;

    let job_id = response.into_inner().job_id;
    println!("Submitted batch job {}", job_id);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sbatch_directives() {
        let script = r#"#!/bin/bash
#SBATCH --job-name=test
#SBATCH -N 4
#SBATCH --time=4:00:00
#SBATCH --gres=gpu:mi300x:8

echo "hello world"
"#;
        let args = parse_sbatch_directives(script);
        assert_eq!(
            args,
            vec![
                "--job-name=test",
                "-N",
                "4",
                "--time=4:00:00",
                "--gres=gpu:mi300x:8"
            ]
        );
    }

    #[test]
    fn test_parse_memory() {
        assert_eq!(parse_memory_mb("4G").unwrap(), 4096);
        assert_eq!(parse_memory_mb("4096M").unwrap(), 4096);
        assert_eq!(parse_memory_mb("4096").unwrap(), 4096);
        assert_eq!(parse_memory_mb("1024K").unwrap(), 1);
    }

    #[test]
    fn test_pbs_conversion() {
        assert_eq!(
            convert_pbs_to_sbatch("-N myname"),
            Some("--job-name=myname".into())
        );
        assert_eq!(
            convert_pbs_to_sbatch("-l walltime=4:00:00"),
            Some("--time=4:00:00".into())
        );
    }

    // --- #143: CLI args override #SBATCH directives ---
    //
    // Regression: clap previously rejected duplicate scalar args with
    // "the argument '--nodes <NODES>' cannot be used multiple times".
    // overrides_with = "self" makes scalars last-wins.

    fn parse_merged(directives: &[&str], cli: &[&str]) -> SbatchArgs {
        let directives: Vec<String> = directives.iter().map(|s| s.to_string()).collect();
        let cli: Vec<String> = cli.iter().map(|s| s.to_string()).collect();
        let merged = merge_directives_and_cli(&directives, &cli);
        SbatchArgs::try_parse_from(&merged).expect("parse failed")
    }

    #[test]
    fn test_cli_overrides_directive_long_form() {
        // Reproduces the exact scenario from #143.
        let args = parse_merged(&["--nodes=2"], &["sbatch", "--nodes=4"]);
        assert_eq!(args.nodes, 4, "CLI must override directive");
    }

    #[test]
    fn test_cli_overrides_directive_short_form() {
        let args = parse_merged(&["-N", "2"], &["sbatch", "-N", "8"]);
        assert_eq!(args.nodes, 8);
    }

    #[test]
    fn test_cli_overrides_directive_mixed_forms() {
        // Directive uses --nodes=N, CLI uses -N N.
        let args = parse_merged(&["--nodes=2"], &["sbatch", "-N", "16"]);
        assert_eq!(args.nodes, 16);
    }

    #[test]
    fn test_cli_overrides_directive_string_arg() {
        let args = parse_merged(
            &["--job-name=from-script"],
            &["sbatch", "--job-name=from-cli"],
        );
        assert_eq!(args.job_name.as_deref(), Some("from-cli"));
    }

    #[test]
    fn test_cli_overrides_directive_bool_flag() {
        // Bool flags: directive sets, CLI re-sets — must not error.
        let args = parse_merged(&["--exclusive"], &["sbatch", "--exclusive"]);
        assert!(args.exclusive);
    }

    #[test]
    fn test_directive_only_when_no_cli_override() {
        let args = parse_merged(&["--nodes=2", "--time=1:00:00"], &["sbatch"]);
        assert_eq!(args.nodes, 2);
        assert_eq!(args.time.as_deref(), Some("1:00:00"));
    }

    #[test]
    fn test_cli_only_when_no_directive() {
        let args = parse_merged(&[], &["sbatch", "--nodes=4"]);
        assert_eq!(args.nodes, 4);
    }

    #[test]
    fn test_vec_args_accumulate_from_both_sources() {
        // Vec args have no overrides_with — they intentionally accumulate.
        let args = parse_merged(
            &["--gres=gpu:mi300x:8"],
            &["sbatch", "--gres=license:fluent:1"],
        );
        assert_eq!(args.gres, vec!["gpu:mi300x:8", "license:fluent:1"]);
    }

    #[test]
    fn test_partial_override_preserves_other_directives() {
        // CLI overrides nodes but leaves time/job-name from directives.
        let args = parse_merged(
            &["--nodes=2", "--time=1:00:00", "--job-name=script-name"],
            &["sbatch", "--nodes=4"],
        );
        assert_eq!(args.nodes, 4);
        assert_eq!(args.time.as_deref(), Some("1:00:00"));
        assert_eq!(args.job_name.as_deref(), Some("script-name"));
    }

    #[test]
    fn test_cli_can_override_default_value_arg() {
        // `--cpus-per-task` has default_value = "1". Verify directive sets it
        // and CLI overrides — no surprises from the default interacting with
        // overrides_with.
        let args = parse_merged(&["--cpus-per-task=4"], &["sbatch", "--cpus-per-task=8"]);
        assert_eq!(args.cpus_per_task, 8);
    }
}
