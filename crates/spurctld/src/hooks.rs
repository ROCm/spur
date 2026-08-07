// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Embedded Lua backend for the job-submission hook. Kept in spurctld (not
//! spur-core) so the Lua interpreter and its vendored C toolchain ship only with
//! the controller — the only binary that runs submit hooks — and never leak into
//! spurd, the CLI, or the FFI shared object. The shared outcome/whitelist/apply
//! types and the shell backend live in `spur_core::hooks`.

use anyhow::Context;
use spur_core::hooks::{
    changes_from_map, read_secure_hook_file, require_absolute_hook_path, require_secure_hook_file,
    SubmitHookChanges, SubmitHookContext, SubmitHookOutcome, SUBMIT_HOOK_WHITELIST,
};
use tracing::{info, warn};

/// Memory ceiling for a job_submit lua script (a runaway policy must not OOM the
/// controller). Generous for policy logic; not a user-tunable.
const LUA_MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// Instruction budget before a lua script is interrupted (guards infinite loops).
const LUA_INSTRUCTION_LIMIT: u32 = 100_000_000;
/// Base globals that reach the filesystem or the bytecode loader; removed so a
/// sandboxed script cannot read/execute on-disk Lua even without `os`/`io`.
const LUA_UNSAFE_GLOBALS: &[&str] = &["dofile", "loadfile", "load", "loadstring", "collectgarbage"];

/// Compile-check a Lua hook without running it: used at config load / reconfigure
/// so a syntax error is caught at startup, not on the first user submission.
pub fn validate_lua_hook(script_path: &str) -> anyhow::Result<()> {
    require_absolute_hook_path(script_path)?;
    let source = read_secure_hook_file(script_path)?;
    let lua = sandboxed_lua()?;
    lua.load(source.as_str())
        .set_name("job_submit.lua")
        .set_mode(mlua::chunk::ChunkMode::Text)
        .into_function()
        .map_err(|e| lua_err("compile job_submit lua script", e))?;
    Ok(())
}

/// Build the sandboxed Lua VM used for both compiling (validate) and running
/// (submit) hooks, so a compile-time check can never diverge from runtime trust.
fn sandboxed_lua() -> anyhow::Result<mlua::Lua> {
    use mlua::{Lua, StdLib};
    let safe_libs =
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE;
    let lua = Lua::new_with(safe_libs, mlua::LuaOptions::default())
        .map_err(|e| lua_err("initialize sandboxed Lua", e))?;
    harden_lua_sandbox(&lua)?;
    Ok(lua)
}

/// Validate configured submit hooks at startup / reconfigure so a bad path is a
/// loud, early error instead of an opaque failure on the first user submission.
/// The shell hook must be an absolute, secure, executable regular file; the Lua
/// hook must additionally compile.
pub fn validate_submit_hooks(hooks: &spur_core::config::HooksConfig) -> anyhow::Result<()> {
    if let Some(path) = hooks.job_submit.as_deref() {
        require_absolute_hook_path(path)?;
        require_secure_hook_file(path)?;
        require_executable_regular_file(path)?;
    }
    if let Some(path) = hooks.job_submit_lua.as_deref() {
        validate_lua_hook(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn require_executable_regular_file(path: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta =
        std::fs::metadata(path).with_context(|| format!("job_submit hook not found: {path}"))?;
    if !meta.is_file() {
        anyhow::bail!("job_submit hook is not a regular file: {path}");
    }
    if meta.permissions().mode() & 0o111 == 0 {
        anyhow::bail!("job_submit hook is not executable: {path}");
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_executable_regular_file(path: &str) -> anyhow::Result<()> {
    if !std::path::Path::new(path).is_file() {
        anyhow::bail!("job_submit hook is not a regular file: {path}");
    }
    Ok(())
}

/// Run the Lua job_submit hook (Slurm `job_submit/lua` parity): the script defines
/// `slurm_job_submit(job_desc, submit_uid)`. Sandboxed (see [`harden_lua_sandbox`]).
pub fn run_submit_hook_lua(
    script_path: &str,
    ctx: &SubmitHookContext,
) -> anyhow::Result<SubmitHookOutcome> {
    use mlua::{LuaSerdeExt, Value};

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

    let source = read_secure_hook_file(script_path)?;
    let lua = sandboxed_lua()?;

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
        .set_mode(mlua::chunk::ChunkMode::Text)
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
            .map(|m| spur_core::hooks::cap_hook_reason(&m))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_lua(body: &str) -> tempfile::TempPath {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{body}").unwrap();
        f.into_temp_path()
    }

    // Precompiled Lua bytecode is a well-known VM sandbox escape (the loader
    // doesn't verify it); `ChunkMode::Text` must make both load sites refuse it.
    fn make_lua_bytecode(body: &str) -> tempfile::TempPath {
        let compiler = mlua::Lua::new();
        let function = compiler.load(body).into_function().unwrap();
        let bytecode = function.dump(false);
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&bytecode).unwrap();
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

    // A syntactically valid script compiles at config-load time.
    #[test]
    fn validate_lua_hook_accepts_valid_script() {
        let lua = make_lua("function slurm_job_submit(j,u)\n  return slurm.SUCCESS\nend");
        assert!(validate_lua_hook(lua.to_str().unwrap()).is_ok());
    }

    // A syntax error is caught at validation, not deferred to first submission.
    #[test]
    fn validate_lua_hook_rejects_syntax_error() {
        let lua = make_lua("function slurm_job_submit(  this is not lua");
        assert!(validate_lua_hook(lua.to_str().unwrap()).is_err());
    }

    #[test]
    fn lua_sandbox_denies_precompiled_bytecode() {
        let lua = make_lua_bytecode("function slurm_job_submit(j,u)\n  return slurm.SUCCESS\nend");
        let res = run_submit_hook_lua(lua.to_str().unwrap(), &lua_ctx(r#"{"partition":"gpu"}"#));
        assert!(res.is_err(), "precompiled bytecode must be refused");
    }

    #[test]
    fn validate_lua_hook_rejects_precompiled_bytecode() {
        let lua = make_lua_bytecode("function slurm_job_submit(j,u)\n  return slurm.SUCCESS\nend");
        assert!(validate_lua_hook(lua.to_str().unwrap()).is_err());
    }

    // The two tests above go through a UTF-8 file read that incidentally blocks
    // most bytecode; isolate the chunk-mode guard on the raw bytes directly.
    #[test]
    fn chunk_mode_text_rejects_bytecode_even_when_not_utf8_gated() {
        let compiler = mlua::Lua::new();
        let bytecode = compiler
            .load("return 1")
            .into_function()
            .unwrap()
            .dump(false);

        let auto_detected: i64 = mlua::Lua::new()
            .load(bytecode.as_slice())
            .eval()
            .expect("auto-detection executes raw bytecode when not forced to text mode");
        assert_eq!(
            auto_detected, 1,
            "sanity: the dumped bytes are real, runnable bytecode"
        );

        let lua = sandboxed_lua().unwrap();
        let res = lua
            .load(bytecode.as_slice())
            .set_name("job_submit.lua")
            .set_mode(mlua::chunk::ChunkMode::Text)
            .exec();
        assert!(
            res.is_err(),
            "ChunkMode::Text must refuse the same bytecode bytes"
        );
    }
}
