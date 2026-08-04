// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::Stdio;

use anyhow::Context;
use chrono::{DateTime, Utc};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::job::JobSpec;
use crate::spur_env::SpurEnv;

pub type JobId = u32;

/// Context passed to prolog/epilog hook scripts via environment variables.
pub struct HookContext {
    pub job_id: JobId,
    pub work_dir: String,
    pub uid: u32,
    pub gid: u32,
    pub partition: String,
    pub nodelist: String,
    /// Identifies which hook is running. One of:
    /// `prolog_slurmd`, `epilog_slurmd`, `prolog_slurmctld`, `epilog_slurmctld`,
    /// `prolog_task`, `epilog_task`, `prolog_srun`, `epilog_srun`.
    pub script_context: String,
    pub gpu_devices: Vec<u32>,
    pub cpus: u32,
    pub memory_mb: u64,
}

/// Run a prolog/epilog hook script with rich environment variables.
///
/// Stderr is captured and logged; stdout is discarded.
/// Returns `Err` on script execution failure or non-zero exit.
pub async fn run_hook(script_path: &str, ctx: &HookContext) -> anyhow::Result<()> {
    info!(
        job_id = ctx.job_id,
        hook = %ctx.script_context,
        script = script_path,
        "running hook"
    );

    let username = resolve_username(ctx.uid);

    let gpu_list: String = ctx
        .gpu_devices
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let mut env = SpurEnv::new();
    env.set_with_slurm_twin("SPUR_JOB_ID", ctx.job_id);
    env.set_with_slurm_twin("SPUR_JOB_PARTITION", &ctx.partition);
    env.set_with_slurm_twin("SPUR_JOB_NODELIST", &ctx.nodelist);
    env.set_with_slurm_twin("SPUR_CPUS_ON_NODE", ctx.cpus);
    env.set("SPUR_JOB_USER", &username);
    env.set("SPUR_JOB_UID", ctx.uid);
    env.set("SPUR_JOB_GID", ctx.gid);
    env.set("SPUR_JOB_WORK_DIR", &ctx.work_dir);
    env.set("SPUR_JOB_GPUS", &gpu_list);
    env.set("SPUR_JOB_MEMORY_MB", ctx.memory_mb);
    env.set("SPUR_SCRIPT_CONTEXT", &ctx.script_context);

    let mut cmd = Command::new(script_path);
    for (k, v) in env.into_map() {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let child = spawn_hook_in_work_dir(&mut cmd, &ctx.work_dir, ctx.job_id, &ctx.script_context)
        .with_context(|| {
            format!(
                "{} script failed to execute: {}",
                ctx.script_context, script_path
            )
        })?;

    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("{} script failed to complete", ctx.script_context))?;

    if !output.stderr.is_empty() {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        for line in stderr_text.lines() {
            warn!(
                job_id = ctx.job_id,
                hook = %ctx.script_context,
                "{}", line
            );
        }
    }

    if !output.status.success() {
        anyhow::bail!(
            "{} script exited with {} (script: {})",
            ctx.script_context,
            output.status,
            script_path
        );
    }

    Ok(())
}

/// Context for the job-submission hook. Feeds env twins and the audit line;
/// `spec_json` is the fully-resolved spec sent to the script on stdin.
pub struct SubmitHookContext {
    pub spec_json: String,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub partition: String,
}

/// Whitelisted spec fields a submit hook may change. Identity, script/argv, and
/// resource-count fields are absent by construction, so a hook cannot forge them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SubmitHookChanges {
    pub qos: Option<String>,
    pub partition: Option<String>,
    pub account: Option<String>,
    pub constraint: Option<String>,
    pub comment: Option<String>,
    pub reservation: Option<String>,
    pub priority: Option<u32>,
    pub time_limit_minutes: Option<i64>,
    pub begin_time: Option<DateTime<Utc>>,
    pub gres: Option<Vec<String>>,
    pub hold: Option<bool>,
}

/// Decision returned by a job-submission hook.
#[derive(Debug)]
pub enum SubmitHookOutcome {
    Accept,
    Reject(String),
    Modify(SubmitHookChanges),
}

const SUBMIT_HOOK_WHITELIST: &[&str] = &[
    "qos",
    "partition",
    "account",
    "constraint",
    "comment",
    "reservation",
    "priority",
    "time_limit_minutes",
    "begin_time",
    "gres",
    "hold",
];

/// Wall-clock ceiling for a shell job_submit hook; a hung hook is killed and the
/// submission fails closed rather than stalling the controller.
const SUBMIT_HOOK_TIMEOUT_SECS: u64 = 30;
/// Max bytes captured from the hook's stdout/stderr each; a chatty hook can't
/// grow controller memory without bound.
const SUBMIT_HOOK_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Reject a non-absolute hook path: a bare name would resolve via `$PATH` and
/// silently run the wrong binary. The config contract requires a fully-qualified path.
fn require_absolute_hook_path(script_path: &str) -> anyhow::Result<()> {
    if !std::path::Path::new(script_path).is_absolute() {
        anyhow::bail!("job_submit hook path must be absolute: {script_path}");
    }
    Ok(())
}

/// Run the job-submission hook: spec as JSON on stdin; non-zero exit = reject
/// (stderr to user), exit 0 blank = accept, exit 0 + JSON = modify, else `Err`.
pub async fn run_submit_hook(
    script_path: &str,
    ctx: &SubmitHookContext,
) -> anyhow::Result<SubmitHookOutcome> {
    require_absolute_hook_path(script_path)?;
    info!(
        target: "audit",
        hook = "job_submit",
        script = script_path,
        user = %ctx.user,
        uid = ctx.uid,
        partition = %ctx.partition,
        "running job_submit hook"
    );

    let mut env = SpurEnv::new();
    env.set_with_slurm_twin("SPUR_JOB_PARTITION", &ctx.partition);
    env.set("SPUR_JOB_USER", &ctx.user);
    env.set("SPUR_JOB_UID", ctx.uid);
    env.set("SPUR_JOB_GID", ctx.gid);
    env.set("SPUR_SCRIPT_CONTEXT", "job_submit");

    let mut cmd = Command::new(script_path);
    for (k, v) in env.into_map() {
        cmd.env(k, v);
    }
    cmd.current_dir("/tmp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("job_submit script failed to execute: {script_path}"))?;

    // Drain output concurrently with the stdin write: a multi-MB spec plus a
    // script that emits before consuming stdin would otherwise deadlock.
    let mut stdin = child
        .stdin
        .take()
        .context("job_submit stdin was not captured")?;
    let mut stdout = child
        .stdout
        .take()
        .context("job_submit stdout was not captured")?;
    let mut stderr = child
        .stderr
        .take()
        .context("job_submit stderr was not captured")?;
    let spec_bytes = ctx.spec_json.clone().into_bytes();
    let writer = async move {
        // A hook that ignores stdin closes the pipe early; a broken pipe there
        // is expected, not a failure.
        if let Err(e) = stdin.write_all(&spec_bytes).await {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(e);
            }
        }
        stdin.shutdown().await.or_else(ignore_broken_pipe)
    };

    let timeout = std::time::Duration::from_secs(SUBMIT_HOOK_TIMEOUT_SECS);
    let collected = tokio::time::timeout(timeout, async {
        tokio::join!(
            writer,
            read_capped(&mut stdout, SUBMIT_HOOK_MAX_OUTPUT_BYTES),
            read_capped(&mut stderr, SUBMIT_HOOK_MAX_OUTPUT_BYTES),
            child.wait(),
        )
    })
    .await;
    let (write_res, out_bytes, err_bytes, status) = match collected {
        Ok(tuple) => tuple,
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!(
                "job_submit hook timed out after {SUBMIT_HOOK_TIMEOUT_SECS}s (script: {script_path})"
            );
        }
    };
    write_res.context("failed to write spec to job_submit stdin")?;
    let out_bytes = out_bytes.context("failed to read job_submit stdout")?;
    let err_bytes = err_bytes.context("failed to read job_submit stderr")?;
    let status = status.context("job_submit script failed to complete")?;

    let stderr_text = String::from_utf8_lossy(&err_bytes);
    for line in stderr_text.lines() {
        warn!(target: "audit", hook = "job_submit", "{}", line);
    }

    if !status.success() {
        let reason = stderr_text.trim();
        let reason = if reason.is_empty() {
            format!("job rejected by job_submit hook (exit {status})")
        } else {
            reason.to_string()
        };
        return Ok(SubmitHookOutcome::Reject(reason));
    }

    let stdout_text = String::from_utf8_lossy(&out_bytes);
    if stdout_text.trim().is_empty() {
        return Ok(SubmitHookOutcome::Accept);
    }

    let changes = parse_submit_changes(stdout_text.trim())?;
    Ok(SubmitHookOutcome::Modify(changes))
}

/// Read up to `cap` bytes from `reader`; excess is left unread (the child then
/// blocks on a full pipe and is caught by the caller's wall-clock timeout).
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    reader.take(cap as u64).read_to_end(&mut buf).await?;
    Ok(buf)
}

/// Parse the shell hook's stdout into whitelisted changes; malformed JSON or a
/// wrong type fails closed. Non-whitelisted keys are ignored and logged.
fn parse_submit_changes(stdout: &str) -> anyhow::Result<SubmitHookChanges> {
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(stdout).context("job_submit hook emitted unparseable JSON")?;
    changes_from_map(&map)
}

/// Type-check the whitelisted keys of a JSON object into `SubmitHookChanges`,
/// logging (not applying) non-whitelisted keys. Shared by the shell and Lua paths.
fn changes_from_map(
    map: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<SubmitHookChanges> {
    let mut leftover = Vec::new();
    let mut changes = SubmitHookChanges::default();
    for (key, value) in map {
        match key.as_str() {
            "qos" => changes.qos = Some(take_string(key, value)?),
            "partition" => changes.partition = Some(take_string(key, value)?),
            "account" => changes.account = Some(take_string(key, value)?),
            "constraint" => changes.constraint = Some(take_string(key, value)?),
            "comment" => changes.comment = Some(take_string(key, value)?),
            "reservation" => changes.reservation = Some(take_string(key, value)?),
            "priority" => changes.priority = Some(take_u32(key, value)?),
            "time_limit_minutes" => {
                changes.time_limit_minutes = Some(take_time_limit_minutes(value)?)
            }
            "begin_time" => changes.begin_time = Some(take_datetime(key, value)?),
            "gres" => changes.gres = Some(take_string_vec(key, value)?),
            "hold" => changes.hold = Some(take_bool(key, value)?),
            _ => leftover.push(key.clone()),
        }
    }

    if !leftover.is_empty() {
        warn!(
            target: "audit",
            hook = "job_submit",
            ignored = ?leftover,
            whitelist = ?SUBMIT_HOOK_WHITELIST,
            "job_submit hook set non-whitelisted fields; ignoring them"
        );
    }

    Ok(changes)
}

fn ignore_broken_pipe(e: std::io::Error) -> std::io::Result<()> {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(e)
    }
}

fn take_string(key: &str, value: &serde_json::Value) -> anyhow::Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("job_submit field `{key}` must be a string"))
}

fn take_bool(key: &str, value: &serde_json::Value) -> anyhow::Result<bool> {
    value
        .as_bool()
        .with_context(|| format!("job_submit field `{key}` must be a boolean"))
}

fn take_i64(key: &str, value: &serde_json::Value) -> anyhow::Result<i64> {
    // Lua arithmetic yields floats (`x / 2`), so accept a whole-valued float too.
    if let Some(f) = value.as_f64() {
        if value.as_i64().is_none() && f.fract() == 0.0 && f.is_finite() {
            return Ok(f as i64);
        }
    }
    value
        .as_i64()
        .with_context(|| format!("job_submit field `{key}` must be an integer"))
}

/// Fail closed on a walltime a hook must not set: negative (would slip past the
/// partition max-time cap) or so large it overflows `Duration::try_minutes`.
fn take_time_limit_minutes(value: &serde_json::Value) -> anyhow::Result<i64> {
    let minutes = take_i64("time_limit_minutes", value)?;
    if minutes < 0 {
        anyhow::bail!("job_submit field `time_limit_minutes` must not be negative");
    }
    if chrono::Duration::try_minutes(minutes).is_none() {
        anyhow::bail!("job_submit field `time_limit_minutes` is out of range");
    }
    Ok(minutes)
}

fn take_u32(key: &str, value: &serde_json::Value) -> anyhow::Result<u32> {
    let n = take_i64(key, value)?;
    u32::try_from(n).with_context(|| format!("job_submit field `{key}` is out of range for u32"))
}

fn take_string_vec(key: &str, value: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    let arr = value
        .as_array()
        .with_context(|| format!("job_submit field `{key}` must be an array of strings"))?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .with_context(|| format!("job_submit field `{key}` must be an array of strings"))
        })
        .collect()
}

fn take_datetime(key: &str, value: &serde_json::Value) -> anyhow::Result<DateTime<Utc>> {
    let s = take_string(key, value)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .with_context(|| format!("job_submit field `{key}` must be an RFC3339 timestamp"))
}

/// Memory ceiling for a job_submit lua script (a runaway policy must not OOM the
/// controller). Generous for policy logic; not a user-tunable.
const LUA_MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// Instruction budget before a lua script is interrupted (guards infinite loops).
const LUA_INSTRUCTION_LIMIT: u32 = 100_000_000;
/// Base globals that reach the filesystem or the bytecode loader; removed so a
/// sandboxed script cannot read/execute on-disk Lua even without `os`/`io`.
const LUA_UNSAFE_GLOBALS: &[&str] = &["dofile", "loadfile", "load", "loadstring", "collectgarbage"];

/// Run the Lua job_submit hook (Slurm `job_submit/lua` parity): the script defines
/// `slurm_job_submit(job_desc, submit_uid)`. Sandboxed (see [`harden_lua_sandbox`]).
pub fn run_submit_hook_lua(
    script_path: &str,
    ctx: &SubmitHookContext,
) -> anyhow::Result<SubmitHookOutcome> {
    use mlua::{Lua, LuaSerdeExt, StdLib, Value};

    require_absolute_hook_path(script_path)?;
    info!(
        target: "audit",
        hook = "job_submit_lua",
        script = script_path,
        user = %ctx.user,
        uid = ctx.uid,
        partition = %ctx.partition,
        "running job_submit lua hook"
    );

    let source = std::fs::read_to_string(script_path)
        .with_context(|| format!("job_submit lua script unreadable: {script_path}"))?;

    let safe_libs =
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE;
    let lua = Lua::new_with(safe_libs, mlua::LuaOptions::default())
        .map_err(|e| lua_err("initialize sandboxed Lua", e))?;
    harden_lua_sandbox(&lua)?;

    let mut spec_value: serde_json::Value =
        serde_json::from_str(&ctx.spec_json).context("failed to decode job spec for lua hook")?;
    // Present time_limit to Lua as integer minutes (Slurm convention), replacing
    // the internal [secs, nanos] encoding the script would not understand.
    if let Some(obj) = spec_value.as_object_mut() {
        let minutes = obj
            .get("time_limit")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|s| s.as_i64())
            .map(|secs| secs / 60);
        match minutes {
            Some(m) => obj.insert("time_limit".into(), m.into()),
            None => obj.insert("time_limit".into(), serde_json::Value::Null),
        };
    }
    // Map JSON null to Lua nil (not the null userdata sentinel) so a script can
    // check unset fields naturally, e.g. `if job_desc.time_limit == nil`.
    let ser_opts = mlua::serde::SerializeOptions::new()
        .serialize_none_to_null(false)
        .serialize_unit_to_null(false);
    let job_desc = lua
        .to_value_with(&spec_value, ser_opts)
        .map_err(|e| lua_err("expose job spec to lua", e))?;

    let rejection: std::rc::Rc<std::cell::RefCell<Option<String>>> = Default::default();
    let slurm = build_slurm_table(&lua, &rejection)?;
    lua.globals()
        .set("slurm", slurm)
        .map_err(|e| lua_err("set slurm global", e))?;

    lua.load(source.as_str())
        .set_name("job_submit.lua")
        .exec()
        .map_err(|e| lua_err("load job_submit lua script", e))?;

    let func: mlua::Function = lua.globals().get("slurm_job_submit").map_err(|_| {
        anyhow::anyhow!("job_submit lua must define slurm_job_submit(job_desc, submit_uid)")
    })?;
    let rc: i64 = func
        .call((&job_desc, ctx.uid))
        .map_err(|e| lua_err("call slurm_job_submit", e))?;

    if rc != 0 {
        let reason = rejection
            .borrow()
            .clone()
            .unwrap_or_else(|| format!("job rejected by job_submit lua hook (rc {rc})"));
        return Ok(SubmitHookOutcome::Reject(reason));
    }

    let job_desc: mlua::Table = match job_desc {
        Value::Table(t) => t,
        _ => anyhow::bail!("job_desc must remain a table"),
    };
    let changes = lua_table_to_changes(&lua, &job_desc, spec_value.as_object())?;
    if changes == SubmitHookChanges::default() {
        Ok(SubmitHookOutcome::Accept)
    } else {
        Ok(SubmitHookOutcome::Modify(changes))
    }
}

/// `mlua::Error` is neither `Send` nor `Sync`, so it cannot cross into `anyhow`
/// directly; flatten it to a string at the boundary.
fn lua_err(what: &str, e: mlua::Error) -> anyhow::Error {
    anyhow::anyhow!("failed to {what}: {e}")
}

/// Close the holes `Lua::new_with` leaves: the always-loaded base library exposes
/// filesystem/bytecode globals; remove them and cap memory + instructions.
fn harden_lua_sandbox(lua: &mlua::Lua) -> anyhow::Result<()> {
    let globals = lua.globals();
    for name in LUA_UNSAFE_GLOBALS {
        globals
            .set(*name, mlua::Value::Nil)
            .map_err(|e| lua_err(&format!("remove unsafe global `{name}`"), e))?;
    }
    lua.set_memory_limit(LUA_MEMORY_LIMIT_BYTES)
        .map_err(|e| lua_err("set lua memory limit", e))?;
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(LUA_INSTRUCTION_LIMIT),
        |_lua, _debug| {
            Err(mlua::Error::runtime(
                "job_submit lua hook exceeded its instruction budget",
            ))
        },
    )
    .map_err(|e| lua_err("set lua instruction hook", e))?;
    Ok(())
}

/// Build the minimal `slurm` table exposed to the Lua script: return-code
/// constants and `log_user`, which records the message shown on rejection.
fn build_slurm_table(
    lua: &mlua::Lua,
    rejection: &std::rc::Rc<std::cell::RefCell<Option<String>>>,
) -> anyhow::Result<mlua::Table> {
    let build = || -> mlua::Result<mlua::Table> {
        let slurm = lua.create_table()?;
        slurm.set("SUCCESS", 0)?;
        slurm.set("ERROR", -1)?;
        slurm.set("FAILURE", -1)?;
        let sink = rejection.clone();
        let log_user = lua.create_function(move |_, msg: String| {
            *sink.borrow_mut() = Some(msg);
            Ok(())
        })?;
        slurm.set("log_user", log_user)?;
        Ok(slurm)
    };
    build().map_err(|e| lua_err("build slurm table", e))
}

/// Diff whitelisted fields on `job_desc` against their pre-call values, reporting
/// only changed ones. Non-whitelisted keys are never read (no identity/resource edits).
fn lua_table_to_changes(
    lua: &mlua::Lua,
    job_desc: &mlua::Table,
    original: Option<&serde_json::Map<String, serde_json::Value>>,
) -> anyhow::Result<SubmitHookChanges> {
    use mlua::LuaSerdeExt;
    let mut map = serde_json::Map::new();
    for key in SUBMIT_HOOK_WHITELIST {
        // time_limit_minutes is surfaced to Lua as `time_limit` (minutes),
        // matching Slurm; read it under that name and map it back.
        let lua_key = if *key == "time_limit_minutes" {
            "time_limit"
        } else {
            key
        };
        let value: mlua::Value = job_desc
            .get(lua_key)
            .map_err(|e| lua_err(&format!("read lua field `{lua_key}`"), e))?;
        let json: serde_json::Value = lua
            .from_value(value)
            .map_err(|e| lua_err(&format!("convert lua field `{lua_key}`"), e))?;
        if json.is_null() {
            continue;
        }
        // Only report fields the script actually changed from their input value.
        let unchanged = original
            .and_then(|o| o.get(lua_key))
            .is_some_and(|orig| orig == &json);
        if unchanged {
            continue;
        }
        map.insert((*key).to_string(), json);
    }
    let ignored = ignored_lua_fields(lua, job_desc, original)?;
    if !ignored.is_empty() {
        warn!(
            target: "audit",
            hook = "job_submit_lua",
            ignored = ?ignored,
            whitelist = ?SUBMIT_HOOK_WHITELIST,
            "job_submit lua hook set non-whitelisted fields; ignoring them"
        );
    }
    changes_from_map(&map)
}

/// Non-whitelisted `job_desc` keys the script added or changed vs its input
/// (which also lives in `job_desc`), so only script-set keys are surfaced.
fn ignored_lua_fields(
    lua: &mlua::Lua,
    job_desc: &mlua::Table,
    original: Option<&serde_json::Map<String, serde_json::Value>>,
) -> anyhow::Result<Vec<String>> {
    use mlua::LuaSerdeExt;
    let mut ignored = Vec::new();
    for pair in job_desc.pairs::<String, mlua::Value>() {
        let (key, value) = pair.map_err(|e| lua_err("iterate lua job_desc", e))?;
        // `time_limit` is the Lua name of the whitelisted `time_limit_minutes`.
        if SUBMIT_HOOK_WHITELIST.contains(&key.as_str()) || key == "time_limit" {
            continue;
        }
        let json: serde_json::Value = lua.from_value(value).unwrap_or(serde_json::Value::Null);
        let unchanged = original
            .and_then(|o| o.get(&key))
            .is_some_and(|o| o == &json);
        if !unchanged {
            ignored.push(key);
        }
    }
    ignored.sort();
    Ok(ignored)
}

/// Apply whitelisted hook changes onto the spec, returning the names of the
/// fields actually changed (for the audit line). Only `Some` fields are touched.
pub fn apply_submit_changes(spec: &mut JobSpec, changes: &SubmitHookChanges) -> Vec<&'static str> {
    let mut modified = Vec::new();
    // Empty is ignored for these three: blanking them would skip the existence /
    // ACL checks that treat an empty value as "unset, nothing to validate".
    if let Some(qos) = changes.qos.as_deref().filter(|s| !s.is_empty()) {
        spec.qos = Some(qos.to_string());
        modified.push("qos");
    }
    if let Some(partition) = changes.partition.as_deref().filter(|s| !s.is_empty()) {
        spec.partition = Some(partition.to_string());
        modified.push("partition");
    }
    if let Some(account) = changes.account.as_deref().filter(|s| !s.is_empty()) {
        spec.account = Some(account.to_string());
        modified.push("account");
    }
    if let Some(ref constraint) = changes.constraint {
        spec.constraint = Some(constraint.clone());
        modified.push("constraint");
    }
    if let Some(ref comment) = changes.comment {
        spec.comment = Some(comment.clone());
        modified.push("comment");
    }
    if let Some(ref reservation) = changes.reservation {
        spec.reservation = Some(reservation.clone());
        modified.push("reservation");
    }
    if let Some(priority) = changes.priority {
        spec.priority = Some(priority);
        modified.push("priority");
    }
    // Validated in `take_time_limit_minutes`; `try_minutes` keeps this panic-free
    // even if that guard is ever bypassed.
    if let Some(dur) = changes
        .time_limit_minutes
        .and_then(chrono::Duration::try_minutes)
    {
        spec.time_limit = Some(dur);
        modified.push("time_limit");
    }
    if let Some(begin_time) = changes.begin_time {
        spec.begin_time = Some(begin_time);
        modified.push("begin_time");
    }
    if let Some(ref gres) = changes.gres {
        spec.gres = gres.clone();
        modified.push("gres");
    }
    if let Some(hold) = changes.hold {
        spec.hold = hold;
        modified.push("hold");
    }
    modified
}

/// Spawn a hook in `work_dir`, retrying from `/tmp` if the spawn fails there.
/// A missing/untraversable `work_dir` must not fail the hook (spurd drains the
/// node on hook failure); only a failure that also persists from `/tmp` is real.
fn spawn_hook_in_work_dir(
    cmd: &mut Command,
    work_dir: &str,
    job_id: JobId,
    script_context: &str,
) -> std::io::Result<tokio::process::Child> {
    if work_dir.is_empty() {
        return cmd.current_dir("/tmp").spawn();
    }
    let first_err = match cmd.current_dir(work_dir).spawn() {
        Ok(child) => return Ok(child),
        Err(e) => e,
    };
    let child = cmd.current_dir("/tmp").spawn()?;
    warn!(
        job_id,
        hook = %script_context,
        work_dir,
        error = %first_err,
        "hook could not start in work_dir, ran from /tmp instead"
    );
    Ok(child)
}

fn resolve_username(uid: u32) -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| uid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_script(body: &str) -> tempfile::TempPath {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "#!/bin/bash\n{}", body).unwrap();
        let path = f.into_temp_path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    fn test_ctx() -> HookContext {
        HookContext {
            job_id: 42,
            work_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            partition: "gpu".into(),
            nodelist: "node01".into(),
            script_context: "prolog_slurmd".into(),
            gpu_devices: vec![0, 1],
            cpus: 8,
            memory_mb: 16384,
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_success() {
        let script = make_script("exit 0");
        let ctx = test_ctx();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_failure_returns_error() {
        let script = make_script("exit 1");
        let ctx = test_ctx();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("prolog_slurmd"));
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_nonexistent_script() {
        let ctx = test_ctx();
        let result = run_hook("/nonexistent/hook.sh", &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_receives_env_vars() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let body = format!(
            "echo \"$SPUR_JOB_ID|$SPUR_JOB_UID|$SPUR_JOB_PARTITION|$SPUR_SCRIPT_CONTEXT|$SPUR_JOB_GPUS|$SPUR_CPUS_ON_NODE\" > {}",
            marker_path
        );
        let script = make_script(&body);
        let ctx = test_ctx();
        run_hook(script.to_str().unwrap(), &ctx).await.unwrap();

        let content = std::fs::read_to_string(&marker_path).unwrap();
        let parts: Vec<&str> = content.trim().split('|').collect();
        assert_eq!(parts[0], "42");
        assert_eq!(parts[1], ctx.uid.to_string());
        assert_eq!(parts[2], "gpu");
        assert_eq!(parts[3], "prolog_slurmd");
        assert_eq!(parts[4], "0,1");
        assert_eq!(parts[5], "8");
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_receives_slurm_twins() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let body = format!(
            "echo \"$SLURM_JOB_ID|$SLURM_JOB_PARTITION|$SLURM_JOB_NODELIST|$SLURM_CPUS_ON_NODE\" > {}",
            marker_path
        );
        let script = make_script(&body);
        let ctx = test_ctx();
        run_hook(script.to_str().unwrap(), &ctx).await.unwrap();

        let content = std::fs::read_to_string(&marker_path).unwrap();
        let parts: Vec<&str> = content.trim().split('|').collect();
        assert_eq!(parts[0], "42");
        assert_eq!(parts[1], "gpu");
        assert_eq!(parts[2], "node01");
        assert_eq!(parts[3], "8");
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_stderr_does_not_prevent_success() {
        let script = make_script("echo 'warning message' >&2\nexit 0");
        let ctx = test_ctx();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        assert!(result.is_ok());
    }

    // A missing work_dir must not fail the hook (spurd would drain the node).
    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_with_missing_work_dir_still_runs() {
        let script = make_script("exit 0");
        let mut ctx = test_ctx();
        ctx.work_dir = "/nonexistent/path/that/does/not/exist".into();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        assert!(result.is_ok());
    }

    // An existing-but-untraversable work_dir (NFS root_squash) also fails to
    // spawn, which a stat-based precheck would miss. Root bypasses perm checks.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_with_untraversable_work_dir_still_runs() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let script = make_script("exit 0");
        let mut ctx = test_ctx();
        ctx.work_dir = dir.path().to_string_lossy().into_owned();
        let result = run_hook(script.to_str().unwrap(), &ctx).await;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_runs_in_work_dir_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let marker = NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let body = format!("pwd -P > {}", marker_path);
        let script = make_script(&body);
        let mut ctx = test_ctx();
        ctx.work_dir = canonical.to_string_lossy().into_owned();
        run_hook(script.to_str().unwrap(), &ctx).await.unwrap();
        let content = std::fs::read_to_string(&marker_path).unwrap();
        assert_eq!(content.trim(), canonical.to_string_lossy());
    }

    // SPUR_JOB_WORK_DIR still reports the submitted path when the CWD falls back.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_reports_submitted_work_dir_when_cwd_falls_back() {
        let marker = NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let body = format!("echo \"$SPUR_JOB_WORK_DIR|$(pwd)\" > {}", marker_path);
        let script = make_script(&body);
        let mut ctx = test_ctx();
        ctx.work_dir = "/nonexistent/submitted/dir".into();
        run_hook(script.to_str().unwrap(), &ctx).await.unwrap();

        let content = std::fs::read_to_string(&marker_path).unwrap();
        let parts: Vec<&str> = content.trim().split('|').collect();
        assert_eq!(parts[0], "/nonexistent/submitted/dir");
        assert_eq!(parts[1], "/tmp");
    }

    // A genuinely broken script still errors — the /tmp retry doesn't mask it.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn hook_missing_script_still_errors_after_fallback() {
        let mut ctx = test_ctx();
        ctx.work_dir = "/nonexistent/submitted/dir".into();
        let result = run_hook("/nonexistent/hook_script.sh", &ctx).await;
        assert!(result.is_err());
    }

    fn submit_ctx() -> SubmitHookContext {
        SubmitHookContext {
            spec_json: r#"{"name":"j1","partition":"gpu"}"#.into(),
            user: "alice".into(),
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            partition: "gpu".into(),
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_accept() {
        let script = make_script("exit 0");
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        assert!(matches!(out, SubmitHookOutcome::Accept));
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_reject_surfaces_stderr() {
        let script = make_script("echo 'partition required' >&2\nexit 1");
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Reject(msg) => assert_eq!(msg, "partition required"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_reject_empty_stderr_gets_generic_message() {
        let script = make_script("exit 3");
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Reject(msg) => assert!(msg.contains("job_submit hook")),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_modify_whitelisted_field() {
        let script = make_script(r#"echo '{"qos":"high"}'"#);
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.qos.as_deref(), Some("high")),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_ignores_non_whitelisted_field() {
        let script = make_script(r#"echo '{"uid":0,"qos":"high"}'"#);
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.qos.as_deref(), Some("high")),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    // A hook that exits 0 but emits garbage is a misconfiguration; fail closed
    // rather than silently accept and bypass enforcement.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_malformed_json_fails_closed() {
        let script = make_script("echo '{not json'");
        let result = run_submit_hook(script.to_str().unwrap(), &submit_ctx()).await;
        assert!(result.is_err());
    }

    // A whitelisted key with the wrong type also fails closed.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_wrong_type_fails_closed() {
        let script = make_script(r#"echo '{"priority":"high"}'"#);
        let result = run_submit_hook(script.to_str().unwrap(), &submit_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_receives_spec_on_stdin() {
        let marker = NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_str().unwrap().to_string();
        let script = make_script(&format!("cat > {marker_path}"));
        run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        let content = std::fs::read_to_string(&marker_path).unwrap();
        assert!(content.contains("\"partition\":\"gpu\""));
    }

    // A hook that never reads stdin closes the pipe early; the broken-pipe write
    // must be tolerated, not surfaced as a failure.
    #[tokio::test]
    #[serial(run_hooks)]
    async fn submit_hook_that_ignores_stdin_still_succeeds() {
        let script = make_script(r#"echo '{"qos":"high"}'"#);
        let out = run_submit_hook(script.to_str().unwrap(), &submit_ctx())
            .await
            .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.qos.as_deref(), Some("high")),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    #[test]
    fn apply_changes_sets_all_whitelisted_and_leaves_identity() {
        let mut spec = JobSpec {
            user: "alice".into(),
            uid: 1000,
            script: Some("run.sh".into()),
            num_nodes: 4,
            ..Default::default()
        };
        let begin = DateTime::parse_from_rfc3339("2026-08-03T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let changes = SubmitHookChanges {
            qos: Some("high".into()),
            partition: Some("gpu".into()),
            account: Some("research".into()),
            constraint: Some("mi300x".into()),
            comment: Some("audited".into()),
            reservation: Some("resv1".into()),
            priority: Some(500),
            time_limit_minutes: Some(120),
            begin_time: Some(begin),
            gres: Some(vec!["gpu:mi300x:4".into()]),
            hold: Some(true),
        };
        let modified = apply_submit_changes(&mut spec, &changes);

        assert_eq!(spec.qos.as_deref(), Some("high"));
        assert_eq!(spec.partition.as_deref(), Some("gpu"));
        assert_eq!(spec.account.as_deref(), Some("research"));
        assert_eq!(spec.constraint.as_deref(), Some("mi300x"));
        assert_eq!(spec.comment.as_deref(), Some("audited"));
        assert_eq!(spec.reservation.as_deref(), Some("resv1"));
        assert_eq!(spec.priority, Some(500));
        assert_eq!(spec.time_limit, Some(chrono::Duration::minutes(120)));
        assert_eq!(spec.begin_time, Some(begin));
        assert_eq!(spec.gres, vec!["gpu:mi300x:4".to_string()]);
        assert!(spec.hold);
        for field in [
            "qos",
            "partition",
            "account",
            "constraint",
            "comment",
            "reservation",
            "priority",
            "time_limit",
            "begin_time",
            "gres",
            "hold",
        ] {
            assert!(modified.contains(&field), "missing {field}");
        }

        assert_eq!(spec.user, "alice");
        assert_eq!(spec.uid, 1000);
        assert_eq!(spec.script.as_deref(), Some("run.sh"));
        assert_eq!(spec.num_nodes, 4);
    }

    #[test]
    fn apply_changes_ignores_empty_partition_qos_account() {
        let mut spec = JobSpec {
            partition: Some("gpu".into()),
            qos: Some("high".into()),
            account: Some("acct".into()),
            ..Default::default()
        };
        let changes = SubmitHookChanges {
            partition: Some(String::new()),
            qos: Some(String::new()),
            account: Some(String::new()),
            ..Default::default()
        };
        let modified = apply_submit_changes(&mut spec, &changes);
        assert!(modified.is_empty());
        assert_eq!(spec.partition.as_deref(), Some("gpu"));
        assert_eq!(spec.qos.as_deref(), Some("high"));
        assert_eq!(spec.account.as_deref(), Some("acct"));
    }

    #[test]
    fn parse_changes_handles_all_typed_fields() {
        let json = r#"{
            "priority": 7,
            "time_limit_minutes": 90,
            "begin_time": "2026-08-03T12:00:00Z",
            "gres": ["gpu:mi300x:2"],
            "hold": true
        }"#;
        let c = parse_submit_changes(json).unwrap();
        assert_eq!(c.priority, Some(7));
        assert_eq!(c.time_limit_minutes, Some(90));
        assert!(c.begin_time.is_some());
        assert_eq!(c.gres.as_deref(), Some(&["gpu:mi300x:2".to_string()][..]));
        assert_eq!(c.hold, Some(true));
    }

    fn make_lua(body: &str) -> tempfile::TempPath {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{body}").unwrap();
        f.into_temp_path()
    }

    fn lua_ctx(spec_json: &str) -> SubmitHookContext {
        SubmitHookContext {
            spec_json: spec_json.into(),
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            partition: "gpu".into(),
        }
    }

    #[test]
    fn lua_accept_when_unchanged() {
        let lua = make_lua("function slurm_job_submit(job_desc, uid)\n  return slurm.SUCCESS\nend");
        let out =
            run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#)).unwrap();
        assert!(matches!(out, SubmitHookOutcome::Accept));
    }

    #[test]
    fn lua_reject_with_log_user_message() {
        let lua = make_lua(
            "function slurm_job_submit(job_desc, uid)\n  slurm.log_user('needs a partition')\n  return slurm.ERROR\nend",
        );
        let out =
            run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#)).unwrap();
        match out {
            SubmitHookOutcome::Reject(m) => assert_eq!(m, "needs a partition"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn lua_modify_sets_qos() {
        let lua = make_lua(
            "function slurm_job_submit(job_desc, uid)\n  job_desc.qos = 'high'\n  return slurm.SUCCESS\nend",
        );
        let out =
            run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#)).unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.qos.as_deref(), Some("high")),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    // Unchanged whitelisted fields must not be reported as edits.
    #[test]
    fn lua_untouched_partition_is_not_a_change() {
        let lua = make_lua(
            "function slurm_job_submit(job_desc, uid)\n  job_desc.comment = 'tag'\n  return slurm.SUCCESS\nend",
        );
        let out = run_submit_hook_lua(
            lua.to_str().unwrap(),
            &lua_ctx(r#"{"partition":"gpu","qos":"low"}"#),
        )
        .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => {
                assert_eq!(c.comment.as_deref(), Some("tag"));
                assert!(c.partition.is_none(), "unchanged partition must not appear");
                assert!(c.qos.is_none(), "unchanged qos must not appear");
            }
            other => panic!("expected modify, got {other:?}"),
        }
    }

    // time_limit is surfaced to Lua in minutes (Slurm convention).
    #[test]
    fn lua_time_limit_is_minutes() {
        let lua = make_lua(
            "function slurm_job_submit(job_desc, uid)\n  if job_desc.time_limit > 60 then job_desc.time_limit = 60 end\n  return slurm.SUCCESS\nend",
        );
        // 7200s = 120 min on input; script caps to 60.
        let out = run_submit_hook_lua(
            lua.to_str().unwrap(),
            &lua_ctx(r#"{"partition":"gpu","time_limit":[7200,0]}"#),
        )
        .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.time_limit_minutes, Some(60)),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    // The sandbox omits the os library, so os.execute is unavailable.
    #[test]
    fn lua_sandbox_denies_os_execute() {
        let lua = make_lua(
            "function slurm_job_submit(job_desc, uid)\n  os.execute('touch /tmp/pwned')\n  return slurm.SUCCESS\nend",
        );
        let res = run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#));
        assert!(
            res.is_err(),
            "os.execute must not be callable in the sandbox"
        );
    }

    // The sandbox omits io and package too.
    #[test]
    fn lua_sandbox_denies_io_and_require() {
        for body in [
            "function slurm_job_submit(j,u)\n  io.open('/etc/passwd')\n  return 0\nend",
            "function slurm_job_submit(j,u)\n  require('os')\n  return 0\nend",
        ] {
            let lua = make_lua(body);
            let res =
                run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#));
            assert!(res.is_err(), "sandbox must deny: {body}");
        }
    }

    // The base library always loads, so dofile/loadfile/load must be stripped;
    // otherwise a script could read and execute arbitrary on-disk Lua.
    #[test]
    fn lua_sandbox_denies_filesystem_base_globals() {
        for body in [
            "function slurm_job_submit(j,u)\n  dofile('/etc/hostname')\n  return 0\nend",
            "function slurm_job_submit(j,u)\n  loadfile('/etc/hostname')\n  return 0\nend",
            "function slurm_job_submit(j,u)\n  load('return 1')\n  return 0\nend",
        ] {
            let lua = make_lua(body);
            let res =
                run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#));
            assert!(res.is_err(), "sandbox must deny: {body}");
        }
    }

    #[test]
    fn lua_infinite_loop_is_interrupted() {
        let lua = make_lua(
            "function slurm_job_submit(j,u)\n  while true do end\n  return slurm.SUCCESS\nend",
        );
        let res = run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#));
        assert!(
            res.is_err(),
            "an infinite loop must be interrupted, not hang"
        );
    }

    // Lua arithmetic yields floats; a whole-valued float time_limit is accepted.
    #[test]
    fn lua_time_limit_float_is_accepted() {
        let lua = make_lua(
            "function slurm_job_submit(j,u)\n  j.time_limit = j.time_limit / 2\n  return slurm.SUCCESS\nend",
        );
        let out = run_submit_hook_lua(
            lua.to_str().unwrap(),
            &lua_ctx(r#"{"partition":"gpu","time_limit":[7200,0]}"#),
        )
        .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.time_limit_minutes, Some(60)),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    // An untouched time_limit must not be reported as a change (minutes round-trip).
    #[test]
    fn lua_untouched_time_limit_is_not_a_change() {
        let lua = make_lua(
            "function slurm_job_submit(j,u)\n  j.comment = 'x'\n  return slurm.SUCCESS\nend",
        );
        let out = run_submit_hook_lua(
            lua.to_str().unwrap(),
            &lua_ctx(r#"{"partition":"gpu","time_limit":[7200,0]}"#),
        )
        .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => {
                assert_eq!(c.comment.as_deref(), Some("x"));
                assert!(
                    c.time_limit_minutes.is_none(),
                    "untouched time_limit leaked"
                );
            }
            other => panic!("expected modify, got {other:?}"),
        }
    }

    // Untouched array (gres) and numeric (priority) fields must not be reported.
    #[test]
    fn lua_untouched_gres_and_priority_not_reported() {
        let lua = make_lua(
            "function slurm_job_submit(j,u)\n  j.comment = 'x'\n  return slurm.SUCCESS\nend",
        );
        let out = run_submit_hook_lua(
            lua.to_str().unwrap(),
            &lua_ctx(r#"{"partition":"gpu","gres":["gpu:mi300x:2"],"priority":50}"#),
        )
        .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => {
                assert!(c.gres.is_none(), "untouched gres leaked");
                assert!(c.priority.is_none(), "untouched priority leaked");
            }
            other => panic!("expected modify, got {other:?}"),
        }
    }

    // A script setting a non-whitelisted field cannot change identity/resources.
    #[test]
    fn lua_non_whitelisted_field_is_ignored() {
        let lua = make_lua(
            "function slurm_job_submit(j,u)\n  j.uid = 0\n  j.num_nodes = 99\n  j.script = '/evil'\n  return slurm.SUCCESS\nend",
        );
        let out = run_submit_hook_lua(
            lua.to_str().unwrap(),
            &lua_ctx(r#"{"partition":"gpu","uid":1000}"#),
        )
        .unwrap();
        assert!(
            matches!(out, SubmitHookOutcome::Accept),
            "non-whitelisted edits must not register as a modify"
        );
    }

    // Only script-added/changed non-whitelisted keys are flagged; an unchanged
    // input field (name) and a whitelisted one (qos) are not.
    #[test]
    fn lua_ignored_fields_detects_only_script_edits() {
        let lua = mlua::Lua::new();
        let job_desc = lua.create_table().unwrap();
        job_desc.set("name", "job1").unwrap(); // unchanged input, non-whitelisted
        job_desc.set("qos", "high").unwrap(); // whitelisted
        job_desc.set("uid", 0).unwrap(); // changed input, non-whitelisted
        job_desc.set("evil", "x").unwrap(); // added, non-whitelisted
        let original = serde_json::json!({"name": "job1", "uid": 1000});
        let ignored = ignored_lua_fields(&lua, &job_desc, original.as_object()).unwrap();
        assert_eq!(ignored, vec!["evil".to_string(), "uid".to_string()]);
    }

    // An unset spec field must read as Lua nil (not a null userdata), so a
    // script can test `if job_desc.time_limit == nil` without a type error.
    #[test]
    fn lua_unset_field_reads_as_nil() {
        let lua = make_lua(
            "function slurm_job_submit(j,u)\n  if j.time_limit == nil then j.comment = 'was-nil' end\n  return slurm.SUCCESS\nend",
        );
        let out = run_submit_hook_lua(
            lua.to_str().unwrap(),
            &lua_ctx(r#"{"partition":"gpu","time_limit":null}"#),
        )
        .unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => assert_eq!(c.comment.as_deref(), Some("was-nil")),
            other => panic!("expected modify, got {other:?}"),
        }
    }

    #[test]
    fn lua_missing_entry_point_errors() {
        let lua = make_lua("local x = 1");
        let res = run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#));
        assert!(res.is_err());
    }

    #[test]
    fn lua_syntax_error_fails_closed() {
        let lua = make_lua("function slurm_job_submit(  this is not lua");
        let res = run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#));
        assert!(res.is_err());
    }

    #[test]
    fn time_limit_minutes_rejects_negative_and_huge() {
        assert!(parse_submit_changes(r#"{"time_limit_minutes": -1}"#).is_err());
        assert!(parse_submit_changes(r#"{"time_limit_minutes": 9223372036854775807}"#).is_err());
        // A sane positive value is still accepted.
        let c = parse_submit_changes(r#"{"time_limit_minutes": 60}"#).unwrap();
        assert_eq!(c.time_limit_minutes, Some(60));
    }

    #[test]
    fn lua_modify_sets_gres_and_begin_time() {
        let lua = make_lua(
            "function slurm_job_submit(j,u)\n  j.gres = {'gpu:mi300x:2'}\n  j.begin_time = '2026-08-04T12:00:00Z'\n  return slurm.SUCCESS\nend",
        );
        let out =
            run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#)).unwrap();
        match out {
            SubmitHookOutcome::Modify(c) => {
                assert_eq!(c.gres.as_deref(), Some(&["gpu:mi300x:2".to_string()][..]));
                assert!(c.begin_time.is_some());
            }
            other => panic!("expected modify, got {other:?}"),
        }
    }

    #[test]
    fn lua_non_integer_return_fails_closed() {
        for ret in ["return 'nope'", "return {}", "return nil"] {
            let lua = make_lua(&format!("function slurm_job_submit(j,u)\n  {ret}\nend"));
            let res =
                run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#));
            assert!(res.is_err(), "non-integer return must fail closed: {ret}");
        }
    }

    #[test]
    fn hook_paths_must_be_absolute() {
        assert!(require_absolute_hook_path("job_submit.sh").is_err());
        assert!(require_absolute_hook_path("./rel/path.sh").is_err());
        assert!(require_absolute_hook_path("/etc/spur/job_submit.sh").is_ok());
    }
}
