// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

/// Errors from hostlist parsing.
#[derive(Debug, Error)]
pub enum HostlistError {
    #[error("invalid hostlist pattern: {0}")]
    InvalidPattern(String),
    #[error("invalid range: {0}")]
    InvalidRange(String),
}

/// Expand a Slurm hostlist pattern into individual hostnames.
///
/// Examples:
/// - `"node[001-003]"` → `["node001", "node002", "node003"]`
/// - `"node[1,3,5-7]"` → `["node1", "node3", "node5", "node6", "node7"]`
/// - `"node001,node002"` → `["node001", "node002"]`
/// - `"gpu[01-04],cpu[01-02]"` → `["gpu01", "gpu02", "gpu03", "gpu04", "cpu01", "cpu02"]`
pub fn expand(pattern: &str) -> Result<Vec<String>, HostlistError> {
    let mut results = Vec::new();
    for part in split_top_level(pattern) {
        expand_single(part.trim(), &mut results)?;
    }
    Ok(results)
}

/// Compress a list of hostnames into a compact hostlist pattern.
///
/// Example: `["node001", "node002", "node003", "node005"]` → `"node[001-003,005]"`
pub fn compress(hosts: &[String]) -> String {
    if hosts.is_empty() {
        return String::new();
    }
    if hosts.len() == 1 {
        return hosts[0].clone();
    }

    // Group by prefix
    let mut groups: Vec<(String, Vec<(u64, usize)>)> = Vec::new();

    for host in hosts {
        if let Some((prefix, num, width)) = split_name_number(host) {
            if let Some(group) = groups.iter_mut().find(|(p, _)| p == &prefix) {
                group.1.push((num, width));
            } else {
                groups.push((prefix, vec![(num, width)]));
            }
        } else {
            // No numeric suffix, standalone
            groups.push((host.clone(), vec![]));
        }
    }

    let mut parts = Vec::new();
    for (prefix, mut nums) in groups {
        if nums.is_empty() {
            parts.push(prefix);
            continue;
        }
        nums.sort_by_key(|(n, _)| *n);
        let width = nums[0].1;
        let ranges = compress_ranges(&nums.iter().map(|(n, _)| *n).collect::<Vec<_>>(), width);
        if nums.len() == 1 {
            parts.push(format!("{}{:0>width$}", prefix, nums[0].0, width = width));
        } else {
            parts.push(format!("{}[{}]", prefix, ranges));
        }
    }

    parts.join(",")
}

/// Split a hostname into (prefix, number, zero-padding width).
fn split_name_number(name: &str) -> Option<(String, u64, usize)> {
    let num_start = name.rfind(|c: char| !c.is_ascii_digit())?;
    let num_str = &name[num_start + 1..];
    if num_str.is_empty() {
        return None;
    }
    let num = num_str.parse::<u64>().ok()?;
    let width = num_str.len();
    Some((name[..num_start + 1].to_string(), num, width))
}

/// Compress a sorted list of numbers into range strings.
fn compress_ranges(nums: &[u64], width: usize) -> String {
    if nums.is_empty() {
        return String::new();
    }

    let mut ranges = Vec::new();
    let mut start = nums[0];
    let mut end = nums[0];

    for &n in &nums[1..] {
        if n == end + 1 {
            end = n;
        } else {
            ranges.push(format_range(start, end, width));
            start = n;
            end = n;
        }
    }
    ranges.push(format_range(start, end, width));
    ranges.join(",")
}

fn format_range(start: u64, end: u64, width: usize) -> String {
    if start == end {
        format!("{:0>width$}", start, width = width)
    } else {
        format!("{:0>width$}-{:0>width$}", start, end, width = width)
    }
}

/// Split a pattern at top-level commas (not inside brackets).
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;

    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Expand a single hostlist term (no top-level commas).
fn expand_single(pattern: &str, results: &mut Vec<String>) -> Result<(), HostlistError> {
    if let Some(bracket_start) = pattern.find('[') {
        let bracket_end = pattern
            .find(']')
            .ok_or_else(|| HostlistError::InvalidPattern("unmatched [".into()))?;

        let prefix = &pattern[..bracket_start];
        let range_str = &pattern[bracket_start + 1..bracket_end];
        let suffix = &pattern[bracket_end + 1..];

        for range_part in range_str.split(',') {
            if let Some(dash) = range_part.find('-') {
                let start_str = &range_part[..dash];
                let end_str = &range_part[dash + 1..];
                let width = start_str.len();
                let start: u64 = start_str
                    .parse()
                    .map_err(|_| HostlistError::InvalidRange(range_part.into()))?;
                let end: u64 = end_str
                    .parse()
                    .map_err(|_| HostlistError::InvalidRange(range_part.into()))?;

                if start > end {
                    return Err(HostlistError::InvalidRange(format!("{} > {}", start, end)));
                }

                for i in start..=end {
                    let name = format!("{}{:0>width$}{}", prefix, i, suffix, width = width);
                    if suffix.contains('[') {
                        expand_single(&name, results)?;
                    } else {
                        results.push(name);
                    }
                }
            } else {
                let name = format!("{}{}{}", prefix, range_part, suffix);
                if suffix.contains('[') {
                    expand_single(&name, results)?;
                } else {
                    results.push(name);
                }
            }
        }
    } else {
        results.push(pattern.to_string());
    }
    Ok(())
}

/// Count the number of hosts in a hostlist pattern without expanding.
pub fn count(pattern: &str) -> Result<usize, HostlistError> {
    // For now, just expand and count. Can optimize later.
    Ok(expand(pattern)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_range() {
        let hosts = expand("node[001-003]").unwrap();
        assert_eq!(hosts, vec!["node001", "node002", "node003"]);
    }

    #[test]
    fn test_comma_separated_ranges() {
        let hosts = expand("node[1,3,5-7]").unwrap();
        assert_eq!(hosts, vec!["node1", "node3", "node5", "node6", "node7"]);
    }

    #[test]
    fn test_multiple_prefixes() {
        let hosts = expand("gpu[01-02],cpu[01-02]").unwrap();
        assert_eq!(hosts, vec!["gpu01", "gpu02", "cpu01", "cpu02"]);
    }

    #[test]
    fn test_single_host() {
        let hosts = expand("login01").unwrap();
        assert_eq!(hosts, vec!["login01"]);
    }

    #[test]
    fn test_plain_comma_list() {
        let hosts = expand("node1,node2,node3").unwrap();
        assert_eq!(hosts, vec!["node1", "node2", "node3"]);
    }

    #[test]
    fn test_compress_basic() {
        let hosts: Vec<String> = vec!["node001", "node002", "node003"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(compress(&hosts), "node[001-003]");
    }

    #[test]
    fn test_compress_with_gap() {
        let hosts: Vec<String> = vec!["node001", "node002", "node003", "node005"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(compress(&hosts), "node[001-003,005]");
    }

    #[test]
    fn test_count() {
        assert_eq!(count("node[001-100]").unwrap(), 100);
        assert_eq!(count("gpu[1-4],cpu[1-8]").unwrap(), 12);
    }

    #[test]
    fn test_roundtrip() {
        let original = "node[001-003,005,010-012]";
        let expanded = expand(original).unwrap();
        let compressed = compress(&expanded);
        let re_expanded = expand(&compressed).unwrap();
        assert_eq!(expanded, re_expanded);
    }
}
