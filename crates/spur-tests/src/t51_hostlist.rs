// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T51: Hostlist expansion and compression.
//!
//! Corresponds to Slurm's slurm_unit/common/hostlist tests.

#[cfg(test)]
mod tests {
    use spur_core::hostlist;

    // ── T51.1: Basic expansion ────────────────────────────────────

    #[test]
    fn t51_1_expand_simple_range() {
        let hosts = hostlist::expand("node[001-003]").unwrap();
        assert_eq!(hosts, vec!["node001", "node002", "node003"]);
    }

    #[test]
    fn t51_2_expand_comma_in_brackets() {
        let hosts = hostlist::expand("node[1,3,5-7]").unwrap();
        assert_eq!(hosts, vec!["node1", "node3", "node5", "node6", "node7"]);
    }

    #[test]
    fn t51_3_expand_multiple_prefixes() {
        let hosts = hostlist::expand("gpu[01-02],cpu[01-02]").unwrap();
        assert_eq!(hosts, vec!["gpu01", "gpu02", "cpu01", "cpu02"]);
    }

    #[test]
    fn t51_4_expand_single_host() {
        let hosts = hostlist::expand("login01").unwrap();
        assert_eq!(hosts, vec!["login01"]);
    }

    #[test]
    fn t51_5_expand_plain_comma_list() {
        let hosts = hostlist::expand("node1,node2,node3").unwrap();
        assert_eq!(hosts, vec!["node1", "node2", "node3"]);
    }

    // ── T51.6: Zero padding ──────────────────────────────────────

    #[test]
    fn t51_6_expand_preserves_zero_padding() {
        let hosts = hostlist::expand("node[0001-0003]").unwrap();
        assert_eq!(hosts, vec!["node0001", "node0002", "node0003"]);
    }

    #[test]
    fn t51_7_expand_no_padding() {
        let hosts = hostlist::expand("n[1-3]").unwrap();
        assert_eq!(hosts, vec!["n1", "n2", "n3"]);
    }

    // ── T51.8: Compression ───────────────────────────────────────

    #[test]
    fn t51_8_compress_contiguous() {
        let hosts: Vec<String> = vec!["node001", "node002", "node003"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(hostlist::compress(&hosts), "node[001-003]");
    }

    #[test]
    fn t51_9_compress_with_gap() {
        let hosts: Vec<String> = vec!["node001", "node002", "node003", "node005"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(hostlist::compress(&hosts), "node[001-003,005]");
    }

    #[test]
    fn t51_10_compress_single() {
        let hosts = vec!["login01".to_string()];
        assert_eq!(hostlist::compress(&hosts), "login01");
    }

    #[test]
    fn t51_11_compress_empty() {
        let hosts: Vec<String> = vec![];
        assert_eq!(hostlist::compress(&hosts), "");
    }

    // ── T51.12: Count ────────────────────────────────────────────

    #[test]
    fn t51_12_count_range() {
        assert_eq!(hostlist::count("node[001-100]").unwrap(), 100);
    }

    #[test]
    fn t51_13_count_mixed() {
        assert_eq!(hostlist::count("gpu[1-4],cpu[1-8]").unwrap(), 12);
    }

    #[test]
    fn t51_14_count_single() {
        assert_eq!(hostlist::count("login01").unwrap(), 1);
    }

    // ── T51.15: Roundtrip ────────────────────────────────────────

    #[test]
    fn t51_15_roundtrip_expand_compress() {
        let original = "node[001-003,005,010-012]";
        let expanded = hostlist::expand(original).unwrap();
        let compressed = hostlist::compress(&expanded);
        let re_expanded = hostlist::expand(&compressed).unwrap();
        assert_eq!(expanded, re_expanded);
    }

    #[test]
    fn t51_16_roundtrip_large_range() {
        let original = "compute[0001-1024]";
        let expanded = hostlist::expand(original).unwrap();
        assert_eq!(expanded.len(), 1024);
        let compressed = hostlist::compress(&expanded);
        let re_expanded = hostlist::expand(&compressed).unwrap();
        assert_eq!(expanded, re_expanded);
    }

    // ── T51.17: Error cases ──────────────────────────────────────

    #[test]
    fn t51_17_invalid_unmatched_bracket() {
        assert!(hostlist::expand("node[001-003").is_err());
    }

    #[test]
    fn t51_18_invalid_reversed_range() {
        assert!(hostlist::expand("node[5-1]").is_err());
    }
}
