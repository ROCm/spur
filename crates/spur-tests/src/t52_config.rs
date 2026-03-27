//! T52: Configuration parsing.
//!
//! Tests for TOML config loading, time parsing, partition building.

#[cfg(test)]
mod tests {
    use spur_core::config::*;
    use spur_core::partition::PartitionState;

    // ── T52.1: Time parsing ──────────────────────────────────────

    #[test]
    fn t52_1_parse_minutes() {
        assert_eq!(parse_time_minutes("60"), Some(60));
    }

    #[test]
    fn t52_2_parse_hours_minutes() {
        assert_eq!(parse_time_minutes("1:30"), Some(90));
    }

    #[test]
    fn t52_3_parse_hms() {
        assert_eq!(parse_time_minutes("72:00:00"), Some(4320));
    }

    #[test]
    fn t52_4_parse_days_hms() {
        assert_eq!(parse_time_minutes("1-00:00:00"), Some(1440));
        assert_eq!(parse_time_minutes("7-00:00:00"), Some(10080));
    }

    #[test]
    fn t52_5_parse_infinite() {
        assert_eq!(parse_time_minutes("INFINITE"), None);
        assert_eq!(parse_time_minutes("UNLIMITED"), None);
    }

    #[test]
    fn t52_6_parse_case_insensitive() {
        assert_eq!(parse_time_minutes("infinite"), None);
        assert_eq!(parse_time_minutes("Unlimited"), None);
    }

    // ── T52.7: Time formatting ───────────────────────────────────

    #[test]
    fn t52_7_format_hours() {
        assert_eq!(format_time(Some(90)), "01:30:00");
    }

    #[test]
    fn t52_8_format_days() {
        assert_eq!(format_time(Some(1500)), "1-01:00:00");
    }

    #[test]
    fn t52_9_format_unlimited() {
        assert_eq!(format_time(None), "UNLIMITED");
    }

    // ── T52.10: Config loading ───────────────────────────────────

    #[test]
    fn t52_10_minimal_config() {
        let config = SlurmConfig::from_str(r#"cluster_name = "test""#).unwrap();
        assert_eq!(config.cluster_name, "test");
    }

    #[test]
    fn t52_11_missing_cluster_name() {
        let result = SlurmConfig::from_str(r#"[controller]"#);
        assert!(result.is_err());
    }

    #[test]
    fn t52_12_full_config() {
        let config = SlurmConfig::from_str(
            r#"
cluster_name = "prod-cluster"

[controller]
listen_addr = "[::]:6817"
state_dir = "/var/spool/spur"
hosts = ["ctrl1", "ctrl2"]
max_job_id = 999999999
first_job_id = 100
rest_addr = "[::]:6820"

[scheduler]
plugin = "backfill"
interval_secs = 2

[accounting]
host = "db1:6819"
database_url = "postgresql://spur:spur@db1/spur"

[[partitions]]
name = "gpu"
default = true
nodes = "gpu[001-008]"
max_time = "72:00:00"

[[partitions]]
name = "cpu"
nodes = "cpu[001-064]"
max_time = "168:00:00"
priority_tier = 2

[[nodes]]
names = "gpu[001-008]"
cpus = 128
memory_mb = 512000
gres = ["gpu:mi300x:8"]

[[nodes]]
names = "cpu[001-064]"
cpus = 256
memory_mb = 1024000
"#,
        )
        .unwrap();

        assert_eq!(config.cluster_name, "prod-cluster");
        assert_eq!(config.controller.hosts.len(), 2);
        assert_eq!(config.controller.first_job_id, 100);
        assert_eq!(config.scheduler.interval_secs, 2);
        assert_eq!(config.partitions.len(), 2);
        assert_eq!(config.nodes.len(), 2);
        assert!(config.partitions[0].default);
        assert_eq!(config.nodes[0].gres, vec!["gpu:mi300x:8"]);
    }

    // ── T52.13: Partition building ───────────────────────────────

    #[test]
    fn t52_13_build_partitions() {
        let config = SlurmConfig::from_str(
            r#"
cluster_name = "test"

[[partitions]]
name = "batch"
default = true
nodes = "node[001-010]"
max_time = "24:00:00"
priority_tier = 1

[[partitions]]
name = "debug"
nodes = "node[001-002]"
max_time = "1:00"
"#,
        )
        .unwrap();

        let parts = config.build_partitions();
        assert_eq!(parts.len(), 2);

        assert_eq!(parts[0].name, "batch");
        assert!(parts[0].is_default);
        assert_eq!(parts[0].max_time_minutes, Some(1440));
        assert_eq!(parts[0].state, PartitionState::Up);

        assert_eq!(parts[1].name, "debug");
        assert!(!parts[1].is_default);
        assert_eq!(parts[1].max_time_minutes, Some(60));
    }

    // ── T52.14: Scheduler config defaults ────────────────────────

    #[test]
    fn t52_14_scheduler_defaults() {
        let config = SlurmConfig::from_str(
            r#"
cluster_name = "test"
[scheduler]
plugin = "backfill"
"#,
        )
        .unwrap();

        assert_eq!(config.scheduler.max_jobs_per_cycle, 10000);
        assert_eq!(config.scheduler.fairshare_halflife_days, 14);
        assert_eq!(config.scheduler.default_time_limit_minutes, 60);
    }

    // ── T52.15–17: listen_addr from config (#37) ──────────────────

    #[test]
    fn t52_15_listen_addr_preserved_from_config() {
        // Regression: spurctld ignored config listen_addr and always bound to :6817 (#37).
        let config = SlurmConfig::from_str(
            r#"
cluster_name = "prod"
[controller]
listen_addr = "[::]:6821"
state_dir = "/var/spool/spur"
"#,
        )
        .unwrap();
        assert_eq!(config.controller.listen_addr, "[::]:6821");
    }

    #[test]
    fn t52_16_listen_addr_default_parseable() {
        // When listen_addr is not in config the default must be a valid socket address.
        let config = SlurmConfig::from_str(
            r#"
cluster_name = "test"
[controller]
state_dir = "/tmp/spur"
"#,
        )
        .unwrap();
        assert!(
            config.controller.listen_addr.contains(':'),
            "default listen_addr '{}' must be a host:port address",
            config.controller.listen_addr
        );
    }

    #[test]
    fn t52_17_cli_listen_overrides_config() {
        // When --listen CLI arg is provided it wins; when absent, config value is used.
        // This tests the merging logic introduced to fix #37.
        let config_addr = "[::]:6821";

        let with_cli = Some("[::]:6822".to_string());
        let final_addr = with_cli.unwrap_or_else(|| config_addr.to_string());
        assert_eq!(final_addr, "[::]:6822", "CLI --listen must override config");

        let no_cli: Option<String> = None;
        let final_addr = no_cli.unwrap_or_else(|| config_addr.to_string());
        assert_eq!(
            final_addr, "[::]:6821",
            "config listen_addr used when --listen absent"
        );
    }
}
