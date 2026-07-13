// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T58: SPANK plugin host tests.
//!
//! Tests the SPANK plugin loading, hook invocation, and plugstack parsing.

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use spur_spank::*;

    // -- T58.1: SpankHost creation ----------------------------------------

    #[test]
    fn t58_1_spank_host_new() {
        let host = SpankHost::new();
        assert_eq!(host.plugin_count(), 0);
    }

    // -- T58.2: Hook symbol names match Slurm convention ------------------

    #[test]
    fn t58_2_hook_symbol_names() {
        assert_eq!(SpankHook::Init.symbol_name(), "slurm_spank_init");
        assert_eq!(SpankHook::TaskExit.symbol_name(), "slurm_spank_task_exit");
        assert_eq!(SpankHook::JobEpilog.symbol_name(), "slurm_spank_job_epilog");
        assert_eq!(SpankHook::TaskInit.symbol_name(), "slurm_spank_task_init");
        assert_eq!(SpankHook::Exit.symbol_name(), "slurm_spank_exit");
    }

    // -- T58.3: Empty host invoke succeeds --------------------------------

    #[test]
    fn t58_3_empty_host_invoke() {
        let host = SpankHost::new();
        let mut handle =
            SpankHandle::new(SpankContext::default(), std::collections::HashMap::new());
        assert!(host.invoke_hook(SpankHook::Init, &mut handle).is_ok());
        assert!(host.invoke_hook(SpankHook::TaskExit, &mut handle).is_ok());
        assert!(host.invoke_hook(SpankHook::JobEpilog, &mut handle).is_ok());
    }

    // -- T58.4: Missing plugstack file returns error ----------------------

    #[test]
    fn t58_4_plugstack_parse_missing_file() {
        let result = parse_plugstack(Path::new("/nonexistent/plugstack.conf"));
        assert!(result.is_err());
    }

    // -- T58.5: Plugstack parsing -----------------------------------------

    #[test]
    fn t58_5_plugstack_parse() {
        let dir = std::env::temp_dir().join("spur_t58_test");
        let _ = std::fs::create_dir_all(&dir);
        let conf_path = dir.join("plugstack.conf");
        let mut f = std::fs::File::create(&conf_path).unwrap();
        writeln!(f, "# SPANK plugins").unwrap();
        writeln!(f, "required /usr/lib/spank/renice.so nice=10").unwrap();
        writeln!(f, "optional /usr/lib/spank/logger.so").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "# another comment").unwrap();
        drop(f);

        let entries = parse_plugstack(&conf_path).unwrap();
        assert_eq!(entries.len(), 2);

        assert!(entries[0].required);
        assert_eq!(entries[0].path, PathBuf::from("/usr/lib/spank/renice.so"));
        assert_eq!(entries[0].args, vec!["nice=10"]);

        assert!(!entries[1].required);
        assert_eq!(entries[1].path, PathBuf::from("/usr/lib/spank/logger.so"));
        assert!(entries[1].args.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- T58.6: Handle carries job context --------------------------------

    #[test]
    fn t58_6_handle_context() {
        let ctx = SpankContext {
            job_id: 100,
            uid: 1000,
            gid: 1000,
            step_id: 0,
            num_nodes: 2,
            node_id: 0,
            local_task_count: 4,
            total_task_count: 8,
            task_pid: 54321,
        };
        let handle = SpankHandle::new(ctx, std::collections::HashMap::new());
        assert_eq!(handle.context.job_id, 100);
        assert_eq!(handle.context.task_pid, 54321);
    }

    // -- T58.7: Load nonexistent plugin fails -----------------------------

    #[test]
    fn t58_7_load_nonexistent_plugin() {
        let mut host = SpankHost::new();
        let result = host.load_plugin(Path::new("/nonexistent/plugin.so"), &[]);
        assert!(result.is_err());
        assert_eq!(host.plugin_count(), 0);
    }

    // -- T58.8: Item IDs match Slurm's enum spank_item --------------------

    #[test]
    fn t58_8_item_ids_match_slurm() {
        use std::os::raw::c_int;
        assert_eq!(SpankItem::JobUid as c_int, 0);
        assert_eq!(SpankItem::JobGid as c_int, 1);
        assert_eq!(SpankItem::JobId as c_int, 2);
        assert_eq!(SpankItem::TaskPid as c_int, 14);
    }

    // -- T58.9: spank_setenv / spank_getenv round-trip --------------------

    #[test]
    fn t58_9_setenv_getenv_roundtrip() {
        use std::ffi::{CStr, CString};
        use std::os::raw::c_int;

        let mut handle =
            SpankHandle::new(SpankContext::default(), std::collections::HashMap::new());
        let var = CString::new("SPANK_TEST").unwrap();
        let val = CString::new("hello").unwrap();

        assert_eq!(
            spank_setenv(&mut handle, var.as_ptr(), val.as_ptr(), 0),
            ESPANK_SUCCESS
        );

        let mut buf = [0i8; 32];
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
        assert_eq!(out, "hello");
    }

    // -- T58.10: job env seeded into the handle is readable via getenv ----

    #[test]
    fn t58_10_seeded_env_visible_to_getenv() {
        use std::ffi::{CStr, CString};
        use std::os::raw::c_int;

        let mut env = std::collections::HashMap::new();
        env.insert("SLURM_JOB_ID".to_string(), "4242".to_string());
        let mut handle = SpankHandle::new(SpankContext::default(), env);

        let var = CString::new("SLURM_JOB_ID").unwrap();
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
        assert_eq!(out, "4242");
    }
}
