// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use spur_proto::proto::{JobSpec, SubmitJobRequest};
use std::collections::HashMap;

/// Submit a batch job script.
#[derive(Parser, Debug)]
#[command(name = "sbatch", about = "Submit a batch job script")]
pub struct SbatchArgs {
    // Precedence is CLI > env (SBATCH_*) > #SBATCH directive, matching Slurm;
    // `resolve_sbatch_args` enforces it. `overrides_with = "self"` gives
    // repeated flags last-wins semantics (gres included). The remaining Vec
    // args (licenses, container_mounts, container_env) have no `overrides_with`
    // because they accumulate by design.
    /// Job name
    #[arg(
        short = 'J',
        long,
        env = "SBATCH_JOB_NAME",
        overrides_with = "job_name"
    )]
    pub job_name: Option<String>,

    /// Partition
    #[arg(
        short = 'p',
        long,
        env = "SBATCH_PARTITION",
        overrides_with = "partition"
    )]
    pub partition: Option<String>,

    /// Account
    #[arg(short = 'A', long, env = "SBATCH_ACCOUNT", overrides_with = "account")]
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
    #[arg(long, env = "SBATCH_MEM", overrides_with = "mem")]
    pub mem: Option<String>,

    /// Memory per CPU
    #[arg(long, env = "SBATCH_MEM_PER_CPU", overrides_with = "mem_per_cpu")]
    pub mem_per_cpu: Option<String>,

    /// Generic resources (e.g., "gpu:4", "gpu:mi300x:8")
    // Matches Slurm: `value_delimiter` keeps a single comma-list cumulative,
    // while `overrides_with` makes a repeated --gres replace instead of append.
    #[arg(
        long,
        env = "SBATCH_GRES",
        value_delimiter = ',',
        overrides_with = "gres"
    )]
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
    #[arg(short = 't', long, env = "SBATCH_TIMELIMIT", overrides_with = "time")]
    pub time: Option<String>,

    /// Minimum time limit
    #[arg(long, overrides_with = "time_min")]
    pub time_min: Option<String>,

    /// Working directory
    #[arg(short = 'D', long, overrides_with = "chdir")]
    pub chdir: Option<String>,

    /// Stdout file
    #[arg(short = 'o', long, env = "SBATCH_OUTPUT", overrides_with = "output")]
    pub output: Option<String>,

    /// Stderr file
    #[arg(short = 'e', long, env = "SBATCH_ERROR", overrides_with = "error")]
    pub error: Option<String>,

    /// QoS
    #[arg(short = 'q', long, env = "SBATCH_QOS", overrides_with = "qos")]
    pub qos: Option<String>,

    /// Job dependency (e.g., "afterok:123")
    #[arg(short = 'd', long, overrides_with = "dependency")]
    pub dependency: Option<String>,

    /// Node list
    #[arg(
        short = 'w',
        long,
        env = "SBATCH_NODELIST",
        overrides_with_all = ["nodelist", "nodefile"]
    )]
    pub nodelist: Option<String>,

    /// Read the node list from a file
    #[arg(
        short = 'F',
        long,
        overrides_with_all = ["nodelist", "nodefile"]
    )]
    pub nodefile: Option<String>,

    /// Exclude nodes
    #[arg(short = 'x', long, env = "SBATCH_EXCLUDE", overrides_with = "exclude")]
    pub exclude: Option<String>,

    /// Required node features (e.g., "mi300x,nvlink")
    #[arg(
        short = 'C',
        long,
        env = "SBATCH_CONSTRAINT",
        overrides_with = "constraint"
    )]
    pub constraint: Option<String>,

    /// Target a named reservation
    #[arg(long, env = "SBATCH_RESERVATION", overrides_with = "reservation")]
    pub reservation: Option<String>,

    /// Job array (e.g., "0-99%10")
    #[arg(short = 'a', long, overrides_with = "array")]
    pub array: Option<String>,

    /// Task distribution (block, cyclic, plane, arbitrary)
    #[arg(
        short = 'm',
        long,
        env = "SBATCH_DISTRIBUTION",
        overrides_with = "distribution"
    )]
    pub distribution: Option<String>,

    /// Heterogeneous job component index (0 = first component)
    #[arg(long, overrides_with = "het_group")]
    pub het_group: Option<u32>,

    /// Burst buffer spec ("capacity=NNN;stage_in:cmd;stage_out:cmd"); capacity in GB
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
    #[arg(
        long,
        env = "SBATCH_EXPORT",
        default_value = "ALL",
        overrides_with = "export"
    )]
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

    /// Print only the job ID on success
    #[arg(long)]
    pub parsable: bool,

    /// Wrap the given command in a minimal shell script (mutually exclusive with a script file)
    #[arg(long, conflicts_with = "script")]
    pub wrap: Option<String>,

    /// The batch script file
    pub script: Option<String>,

    /// Arguments passed to the batch script as $1, $2, etc.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub script_args: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum BodySource {
    Wrap(String),
    File(String),
    Stdin,
}

/// Decide where the batch script body comes from.
///
/// `--wrap` wins; else a script file; else stdin (when piped); else error.
fn choose_body_source(
    wrap: Option<String>,
    script: Option<String>,
    stdin_is_terminal: bool,
) -> Result<BodySource> {
    match (wrap, script) {
        (Some(cmd), _) => Ok(BodySource::Wrap(cmd)),
        (None, Some(path)) => Ok(BodySource::File(path)),
        (None, None) if stdin_is_terminal => bail!("sbatch: no script file specified"),
        (None, None) => Ok(BodySource::Stdin),
    }
}

fn wrap_command_body(cmd: &str) -> String {
    format!("#!/bin/sh\n# This script was created by sbatch --wrap.\n\n{cmd}\n")
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

/// Resolve `SbatchArgs` with Slurm precedence: CLI > env (SBATCH_*) > #SBATCH
/// directive > default.
///
/// clap resolves CLI > env in one parse, but directives fed as argv are tagged
/// `CommandLine` and would outrank env. So we parse CLI and directives
/// separately and merge per field: keep the CLI/env value when set, else the
/// directive.
pub fn resolve_sbatch_args(
    directives: &[String],
    cli_args: &[String],
) -> clap::error::Result<SbatchArgs> {
    let cli_matches = SbatchArgs::command().try_get_matches_from(cli_args)?;
    let cli = SbatchArgs::from_arg_matches(&cli_matches)?;

    let mut directive_argv = vec!["sbatch".to_string()];
    directive_argv.extend(directives.iter().cloned());
    let dir = SbatchArgs::try_parse_from(&directive_argv)?;

    Ok(merge_resolved(&cli_matches, cli, dir))
}

/// True when neither the CLI nor an env var set the field. `value_source` is
/// `None` for absent args with no default, which counts the same as a default.
fn is_default(matches: &clap::ArgMatches, id: &str) -> bool {
    !matches!(
        matches.value_source(id),
        Some(ValueSource::CommandLine) | Some(ValueSource::EnvVariable)
    )
}

/// `cli` already has env values folded in by clap, so falling back to the
/// directive value only when `is_default` holds yields CLI > env > directive.
fn merge_resolved(cli_matches: &clap::ArgMatches, cli: SbatchArgs, dir: SbatchArgs) -> SbatchArgs {
    let mut out = cli;

    macro_rules! fallback {
        ($field:ident, $id:literal) => {
            if is_default(cli_matches, $id) {
                out.$field = dir.$field;
            }
        };
    }

    fallback!(job_name, "job_name");
    fallback!(partition, "partition");
    fallback!(account, "account");
    fallback!(nodes, "nodes");
    fallback!(ntasks, "ntasks");
    fallback!(ntasks_per_node, "ntasks_per_node");
    fallback!(cpus_per_task, "cpus_per_task");
    fallback!(mem, "mem");
    fallback!(mem_per_cpu, "mem_per_cpu");
    fallback!(gpus, "gpus");
    fallback!(gpus_per_node, "gpus_per_node");
    fallback!(time, "time");
    fallback!(time_min, "time_min");
    fallback!(chdir, "chdir");
    fallback!(output, "output");
    fallback!(error, "error");
    fallback!(qos, "qos");
    fallback!(dependency, "dependency");
    if is_default(cli_matches, "nodelist") && is_default(cli_matches, "nodefile") {
        out.nodelist = dir.nodelist;
        out.nodefile = dir.nodefile;
    }
    fallback!(exclude, "exclude");
    fallback!(constraint, "constraint");
    fallback!(reservation, "reservation");
    fallback!(array, "array");
    fallback!(distribution, "distribution");
    fallback!(het_group, "het_group");
    fallback!(bb, "bb");
    fallback!(begin, "begin");
    fallback!(deadline, "deadline");
    fallback!(spread_job, "spread_job");
    fallback!(topology, "topology");
    fallback!(open_mode, "open_mode");
    fallback!(mpi, "mpi");
    fallback!(requeue, "requeue");
    fallback!(exclusive, "exclusive");
    fallback!(hold, "hold");
    fallback!(comment, "comment");
    fallback!(mail_type, "mail_type");
    fallback!(mail_user, "mail_user");
    fallback!(export, "export");
    fallback!(container_image, "container_image");
    fallback!(container_workdir, "container_workdir");
    fallback!(container_name, "container_name");
    fallback!(container_readonly, "container_readonly");
    fallback!(container_mount_home, "container_mount_home");
    fallback!(container_entrypoint, "container_entrypoint");
    fallback!(container_remap_root, "container_remap_root");
    fallback!(controller, "controller");
    fallback!(script, "script");
    // gres replaces rather than accumulates, so it merges like the scalars.
    fallback!(gres, "gres");

    // These repeatable options accumulate directive then CLI values by design.
    out.licenses = concat_vec(dir.licenses, std::mem::take(&mut out.licenses));
    out.container_mounts = concat_vec(
        dir.container_mounts,
        std::mem::take(&mut out.container_mounts),
    );
    out.container_env = concat_vec(dir.container_env, std::mem::take(&mut out.container_env));

    out
}

fn concat_vec(mut directive: Vec<String>, mut cli: Vec<String>) -> Vec<String> {
    directive.append(&mut cli);
    directive
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
            value.and_then(convert_pbs_resource)
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
    let sanitized = spur_net::oci::image_file_stem(image);

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

/// Identify the script file for `#SBATCH` directive pre-parsing.
///
/// Uses a lightweight clap pre-parse to extract the `script` positional,
/// which is reliable even when trailing `script_args` are present.
fn candidate_script_path(cli_args: &[String]) -> Option<String> {
    let pre = SbatchArgs::try_parse_from(cli_args).ok()?;
    if pre.wrap.is_some() {
        return None;
    }
    pre.script
}

/// Default job name: explicit `-J`, else `"wrap"` for wrap-mode, else the
/// script filename, else `"sbatch"` (stdin).
fn default_job_name(job_name: Option<&str>, script: Option<&str>, is_wrap: bool) -> String {
    if let Some(name) = job_name {
        return name.to_string();
    }
    if is_wrap {
        return "wrap".to_string();
    }
    script.unwrap_or("sbatch").to_string()
}

pub async fn main_with_args(cli_args: Vec<String>) -> Result<()> {
    let script_content =
        candidate_script_path(&cli_args).and_then(|p| std::fs::read_to_string(&p).ok());

    let directive_args = script_content
        .as_deref()
        .map(parse_sbatch_directives)
        .unwrap_or_default();

    let mut args = resolve_sbatch_args(&directive_args, &cli_args)?;
    let nodelist = crate::nodelist::resolve(args.nodelist.take(), args.nodefile.take())?;

    // Build the job spec
    let is_wrap = args.wrap.is_some();
    if is_wrap && !args.script_args.is_empty() {
        bail!("sbatch: script arguments may not be used with --wrap");
    }
    let stdin_is_terminal = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let script = match choose_body_source(args.wrap.take(), args.script.clone(), stdin_is_terminal)?
    {
        BodySource::Wrap(cmd) => wrap_command_body(&cmd),
        BodySource::File(path) => std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read script: {}", path))?,
        BodySource::Stdin => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf
        }
    };

    let work_dir = args
        .chdir
        .unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().into());

    let name = default_job_name(args.job_name.as_deref(), args.script.as_deref(), is_wrap);

    // Build GRES list
    let mut gres = args.gres;
    if let Some(gpus) = &args.gpus {
        gres.push(format!("gpu:{}", gpus));
    }
    if let Some(gpn) = &args.gpus_per_node {
        gres.push(format!("gpu:{}", gpn));
    }
    // Don't push licenses into gres here — proto_to_job_spec already folds them in.

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
        script_args: args.script_args.clone(),
        work_dir,
        stdout_path: args.output.unwrap_or_default(),
        stderr_path: args.error.unwrap_or_default(),
        stdin_path: String::new(),
        environment,
        time_limit,
        time_min: None,
        qos: args.qos.unwrap_or_default(),
        priority: 0,
        reservation: args.reservation.unwrap_or_default(),
        dependency: dependencies,
        nodelist: nodelist.unwrap_or_default(),
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
        srun_job: false,
        pty: false,
        initial_winsize: None,
    };

    // Submit to controller
    let channel = spur_client::connect_channel(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    let response = client
        .submit_job(SubmitJobRequest {
            spec: Some(job_spec),
        })
        .await
        .context("job submission failed")?;

    let job_id = response.into_inner().job_id;
    if args.parsable {
        println!("{}", job_id);
    } else {
        println!("Submitted batch job {}", job_id);
    }

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
        resolve_sbatch_args(&directives, &cli).expect("parse failed")
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
        // Repeatable options accumulate directive and CLI values by design.
        let args = parse_merged(
            &["--container-mounts=/a:/a"],
            &["sbatch", "--container-mounts=/b:/b"],
        );
        assert_eq!(args.container_mounts, vec!["/a:/a", "/b:/b"]);
    }

    #[test]
    fn test_repeated_gres_flag_replaces() {
        let args = parse_merged(&[], &["sbatch", "--gres=gpu:2", "--gres=gpu:5"]);
        assert_eq!(args.gres, vec!["gpu:5"]);
    }

    #[test]
    fn test_comma_gres_is_cumulative() {
        let args = parse_merged(&[], &["sbatch", "--gres=gpu:2,fpga:1"]);
        assert_eq!(args.gres, vec!["gpu:2", "fpga:1"]);
    }

    #[test]
    fn test_cli_gres_replaces_directive() {
        let args = parse_merged(&["--gres=gpu:8"], &["sbatch", "--gres=gpu:2"]);
        assert_eq!(args.gres, vec!["gpu:2"]);
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
    fn test_nodefile_directive_is_parsed() {
        let args = parse_merged(&["--nodefile=nodes.txt"], &["sbatch"]);
        assert_eq!(args.nodefile.as_deref(), Some("nodes.txt"));
        assert!(args.nodelist.is_none());
    }

    #[test]
    fn test_cli_nodelist_overrides_nodefile_directive() {
        let args = parse_merged(&["--nodefile=nodes.txt"], &["sbatch", "--nodelist=node001"]);
        assert_eq!(args.nodelist.as_deref(), Some("node001"));
        assert!(args.nodefile.is_none());
    }

    #[test]
    fn test_cli_nodefile_overrides_nodelist_directive() {
        let args = parse_merged(&["--nodelist=node001"], &["sbatch", "-F", "nodes.txt"]);
        assert_eq!(args.nodefile.as_deref(), Some("nodes.txt"));
        assert!(args.nodelist.is_none());
    }

    #[test]
    fn test_cli_can_override_default_value_arg() {
        // `--cpus-per-task` has default_value = "1". Verify directive sets it
        // and CLI overrides — no surprises from the default interacting with
        // overrides_with.
        let args = parse_merged(&["--cpus-per-task=4"], &["sbatch", "--cpus-per-task=8"]);
        assert_eq!(args.cpus_per_task, 8);
    }

    // SBATCH_* env vars provide defaults (CLI > env > directive). These tests
    // mutate process-global env vars, so they run serially and use
    // SbatchEnvGuard to stay independent of the runner's environment.
    use serial_test::serial;

    /// Clears every SBATCH_* var on construction and on drop, so an assertion
    /// panic can never leak env state into the next test in this process.
    struct SbatchEnvGuard;

    impl SbatchEnvGuard {
        fn new() -> Self {
            Self::clear();
            SbatchEnvGuard
        }

        fn set(&self, key: &str, val: &str) {
            std::env::set_var(key, val);
        }

        /// The set of SBATCH_* vars is derived from the `env=` attributes on
        /// SbatchArgs so it cannot drift as flags are added or removed.
        fn clear() {
            for arg in SbatchArgs::command().get_arguments() {
                let Some(env) = arg.get_env() else { continue };
                let name = env.to_string_lossy();
                if name.starts_with("SBATCH_") {
                    std::env::remove_var(name.as_ref());
                }
            }
        }
    }

    impl Drop for SbatchEnvGuard {
        fn drop(&mut self) {
            Self::clear();
        }
    }

    #[test]
    #[serial(env_injection)]
    fn test_env_provides_default() {
        let env = SbatchEnvGuard::new();
        env.set("SBATCH_PARTITION", "gpu");
        let args = parse_merged(&[], &["sbatch"]);
        assert_eq!(args.partition.as_deref(), Some("gpu"));
    }

    #[test]
    #[serial(env_injection)]
    fn test_cli_overrides_env() {
        let env = SbatchEnvGuard::new();
        env.set("SBATCH_PARTITION", "gpu");
        let args = parse_merged(&[], &["sbatch", "--partition=cpu"]);
        assert_eq!(args.partition.as_deref(), Some("cpu"));
    }

    #[test]
    #[serial(env_injection)]
    fn test_env_overrides_directive() {
        let env = SbatchEnvGuard::new();
        env.set("SBATCH_PARTITION", "gpu");
        let args = parse_merged(&["--partition=script-part"], &["sbatch"]);
        assert_eq!(
            args.partition.as_deref(),
            Some("gpu"),
            "env var must override #SBATCH directive"
        );
    }

    #[test]
    #[serial(env_injection)]
    fn test_directive_used_when_no_env_or_cli() {
        let _env = SbatchEnvGuard::new();
        let args = parse_merged(&["--partition=script-part"], &["sbatch"]);
        assert_eq!(args.partition.as_deref(), Some("script-part"));
    }

    #[test]
    #[serial(env_injection)]
    fn test_env_gres_comma_split() {
        let env = SbatchEnvGuard::new();
        env.set("SBATCH_GRES", "gpu:1,license:x");
        let args = parse_merged(&[], &["sbatch"]);
        assert_eq!(args.gres, vec!["gpu:1", "license:x"]);
    }

    #[test]
    #[serial(env_injection)]
    fn test_env_gres_overrides_directive() {
        let env = SbatchEnvGuard::new();
        env.set("SBATCH_GRES", "gpu:2");
        let args = parse_merged(&["--gres=gpu:8"], &["sbatch"]);
        assert_eq!(
            args.gres,
            vec!["gpu:2"],
            "env gres must override #SBATCH directive"
        );
    }

    #[test]
    #[serial(env_injection)]
    fn test_cli_gres_replaces_directive_and_ignores_env() {
        let env = SbatchEnvGuard::new();
        env.set("SBATCH_GRES", "gpu:1");
        let args = parse_merged(&["--gres=gpu:mi300x:8"], &["sbatch", "--gres=gpu:2"]);
        assert_eq!(args.gres, vec!["gpu:2"]);
    }

    // --- sbatch --wrap ---

    #[test]
    fn test_wrap_command_body_matches_slurm() {
        assert_eq!(
            wrap_command_body("echo hi"),
            "#!/bin/sh\n# This script was created by sbatch --wrap.\n\necho hi\n"
        );
    }

    #[test]
    fn test_choose_body_source_wrap_wins() {
        assert_eq!(
            choose_body_source(Some("srun x".into()), None, true).unwrap(),
            BodySource::Wrap("srun x".into())
        );
    }

    #[test]
    fn test_choose_body_source_file() {
        assert_eq!(
            choose_body_source(None, Some("job.sh".into()), true).unwrap(),
            BodySource::File("job.sh".into())
        );
    }

    #[test]
    fn test_choose_body_source_stdin_when_piped() {
        assert_eq!(
            choose_body_source(None, None, false).unwrap(),
            BodySource::Stdin
        );
    }

    #[test]
    fn test_choose_body_source_errors_on_tty_with_no_input() {
        assert!(choose_body_source(None, None, true).is_err());
    }

    #[test]
    fn test_wrap_conflicts_with_script() {
        let res = SbatchArgs::try_parse_from(["sbatch", "--wrap=echo hi", "job.sh"]);
        assert!(res.is_err(), "--wrap and a script file must conflict");
    }

    #[test]
    fn test_candidate_script_path_normal() {
        let args: Vec<String> = vec!["sbatch", "job.sh"]
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(candidate_script_path(&args), Some("job.sh".to_string()));
    }

    #[test]
    fn test_candidate_script_path_with_trailing_args() {
        let args: Vec<String> = vec!["sbatch", "job.sh", "arg1", "arg2"]
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(candidate_script_path(&args), Some("job.sh".to_string()));
    }

    #[test]
    fn test_candidate_script_path_wrap_flag() {
        let args: Vec<String> = vec!["sbatch", "--wrap", "run.sh"]
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(candidate_script_path(&args), None);
    }

    #[test]
    fn test_candidate_script_path_wrap_equals() {
        let args: Vec<String> = vec!["sbatch", "--wrap=echo hi"]
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(candidate_script_path(&args), None);
    }

    #[test]
    fn test_candidate_script_path_trailing_flag() {
        let args: Vec<String> = vec!["sbatch", "--nodes=2"]
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(candidate_script_path(&args), None);
    }

    #[test]
    fn test_candidate_script_path_bare_sbatch() {
        let args: Vec<String> = vec!["sbatch"].into_iter().map(Into::into).collect();
        assert_eq!(candidate_script_path(&args), None);
    }

    #[test]
    fn test_default_job_name_stdin() {
        assert_eq!(default_job_name(None, None, false), "sbatch");
    }

    #[test]
    fn test_default_job_name_wrap() {
        assert_eq!(default_job_name(None, None, true), "wrap");
    }

    #[test]
    fn test_default_job_name_explicit() {
        assert_eq!(default_job_name(Some("mine"), None, false), "mine");
    }

    #[test]
    fn test_default_job_name_explicit_overrides_wrap() {
        assert_eq!(default_job_name(Some("mine"), None, true), "mine");
    }

    #[test]
    fn test_default_job_name_from_script() {
        assert_eq!(default_job_name(None, Some("job.sh"), false), "job.sh");
    }

    #[test]
    fn test_default_job_name_explicit_overrides_script() {
        assert_eq!(
            default_job_name(Some("mine"), Some("job.sh"), false),
            "mine"
        );
    }

    #[test]
    fn test_wrap_job_name_override() {
        let args = SbatchArgs::try_parse_from(["sbatch", "-J", "mine", "--wrap=echo hi"]).unwrap();
        assert_eq!(args.job_name.as_deref(), Some("mine"));
    }

    #[test]
    fn test_wrap_string_not_parsed_for_directives() {
        let directives = parse_sbatch_directives("#SBATCH --nodes=9\n");
        assert_eq!(directives, vec!["--nodes=9"]);
        assert_eq!(
            wrap_command_body("#SBATCH --nodes=9"),
            "#!/bin/sh\n# This script was created by sbatch --wrap.\n\n#SBATCH --nodes=9\n"
        );
    }

    #[test]
    fn test_resolve_container_image_cross_form() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = tempfile::tempdir().unwrap();
        let stem = spur_net::oci::image_file_stem("docker://busybox:latest");
        let image_path = dir.path().join(format!("{}.sqsh", stem));
        std::fs::write(&image_path, b"fake").unwrap();

        let prev = std::env::var_os("SPUR_IMAGE_DIR");
        std::env::set_var("SPUR_IMAGE_DIR", dir.path());

        let resolved = resolve_container_image(Some("busybox"));

        match prev {
            Some(v) => std::env::set_var("SPUR_IMAGE_DIR", v),
            None => std::env::remove_var("SPUR_IMAGE_DIR"),
        }

        assert_eq!(
            resolved,
            image_path.to_string_lossy(),
            "bare 'busybox' must resolve to the image imported as 'docker://busybox:latest'"
        );
    }
}
