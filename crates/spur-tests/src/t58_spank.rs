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
        assert!(host.invoke_hook(SpankHook::Init).is_ok());
        assert!(host.invoke_hook(SpankHook::TaskExit).is_ok());
        assert!(host.invoke_hook(SpankHook::JobEpilog).is_ok());
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
        writeln!(f, "").unwrap();
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

    // -- T58.6: Context can be set ----------------------------------------

    #[test]
    fn t58_6_set_context() {
        let mut host = SpankHost::new();
        host.set_context(SpankContext {
            job_id: 100,
            uid: 1000,
            gid: 1000,
            step_id: 0,
            num_nodes: 2,
            node_id: 0,
            local_task_count: 4,
            total_task_count: 8,
            task_pid: 54321,
        });
        // No panic, context accepted
        assert_eq!(host.plugin_count(), 0);
    }

    // -- T58.7: Load nonexistent plugin fails -----------------------------

    #[test]
    fn t58_7_load_nonexistent_plugin() {
        let mut host = SpankHost::new();
        let result = host.load_plugin(Path::new("/nonexistent/plugin.so"));
        assert!(result.is_err());
        assert_eq!(host.plugin_count(), 0);
    }
}
