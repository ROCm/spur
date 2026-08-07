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
    results.retain(|s| !s.is_empty());
    Ok(results)
}

/// Expand a hostlist pattern only far enough to yield its first hostname.
///
/// Equivalent to `expand(pattern)?.into_iter().next()`, but stops after the
/// first name instead of materializing every host. Useful when only the first
/// allocated node is needed (e.g. connecting to a job's primary node) and the
/// allocation may span thousands of nodes.
pub fn expand_first(pattern: &str) -> Result<Option<String>, HostlistError> {
    for part in split_top_level(pattern) {
        if let Some(host) = first_single(part.trim())? {
            return Ok(Some(host));
        }
    }
    Ok(None)
}

/// Compress a list of hostnames into a compact hostlist pattern.
///
/// Example: `["node001", "node002", "node003", "node005"]` → `"node[001-003,005]"`
///
/// Duplicates are removed and the output is deterministic regardless of input
/// order (prefixes are natural-sorted, matching Slurm). All numbers under one
/// prefix share a single bracket, even across differing zero-padding; paddings
/// that cannot merge are emitted as separate terms so the result always
/// round-trips through [`expand`] (e.g. `["node9", "node010", "node011"]` →
/// `"node[9,010-011]"`).
pub fn compress(hosts: &[String]) -> String {
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<&str> = hosts
        .iter()
        .map(String::as_str)
        .filter(|h| seen.insert(*h))
        .collect();
    if unique.is_empty() {
        return String::new();
    }

    // Numeric hosts group by prefix; hosts without a trailing digit run stay as
    // standalone bare names. A bare name must never merge into a numeric group
    // of the same spelling (or vice versa), otherwise a bare `node` would be
    // swallowed by `node1`'s prefix.
    let mut numeric: Vec<(String, Vec<(u64, usize)>)> = Vec::new();
    let mut bare: Vec<String> = Vec::new();

    for host in unique {
        match split_name_number(host) {
            Some((prefix, num, width)) => {
                if let Some(group) = numeric.iter_mut().find(|(p, _)| *p == prefix) {
                    group.1.push((num, width));
                } else {
                    numeric.push((prefix, vec![(num, width)]));
                }
            }
            None => bare.push(host.to_string()),
        }
    }

    struct Term {
        prefix: String,
        bare: bool,
        min_num: u64,
        rendered: String,
    }
    let mut terms: Vec<Term> = Vec::new();

    for name in bare {
        terms.push(Term {
            prefix: name.clone(),
            bare: true,
            min_num: 0,
            rendered: name,
        });
    }

    for (prefix, mut nums) in numeric {
        nums.sort_unstable();
        let ranges = coalesce_ranges(&nums);
        let min_num = ranges[0].0;
        let rendered = render_prefix(&prefix, &ranges);
        terms.push(Term {
            prefix,
            bare: false,
            min_num,
            rendered,
        });
    }

    // Natural prefix order; a bare name sorts before numeric hosts of the same
    // prefix (Slurm's singlehost-first), then by the term's lowest number.
    terms.sort_by(|a, b| {
        natural_cmp(&a.prefix, &b.prefix)
            .then_with(|| b.bare.cmp(&a.bare))
            .then_with(|| a.min_num.cmp(&b.min_num))
            .then_with(|| a.rendered.cmp(&b.rendered))
    });

    terms
        .into_iter()
        .map(|t| t.rendered)
        .collect::<Vec<_>>()
        .join(",")
}

/// Input must be sorted. Consecutive numbers merge into one range only when
/// contiguous and zero-padding-compatible ([`width_equiv`]); exact duplicates
/// (same value and width) are dropped.
fn coalesce_ranges(nums: &[(u64, usize)]) -> Vec<(u64, u64, usize)> {
    let mut ranges: Vec<(u64, u64, usize)> = Vec::new();
    for &(num, width) in nums {
        if let Some(last) = ranges.last_mut() {
            if num == last.1 && width == last.2 {
                continue;
            }
            if last.1.checked_add(1) == Some(num) {
                if let Some(combined) = width_equiv(last.0, last.2, num, width) {
                    last.1 = num;
                    last.2 = combined;
                    continue;
                }
            }
        }
        ranges.push((num, num, width));
    }
    ranges
}

/// Brackets are omitted for a single one-host range (`prefix5`); every other
/// case is bracketed (`prefix[...]`), matching Slurm's `_is_bracket_needed`.
fn render_prefix(prefix: &str, ranges: &[(u64, u64, usize)]) -> String {
    let body = ranges
        .iter()
        .map(|&(lo, hi, width)| format_range(lo, hi, width))
        .collect::<Vec<_>>()
        .join(",");
    let bracket_needed = ranges.len() > 1 || ranges[0].0 != ranges[0].1;
    if bracket_needed {
        format!("{prefix}[{body}]")
    } else {
        format!("{prefix}{body}")
    }
}

/// Slurm's `_width_equiv`: whether `n`@`wn` and `m`@`wm` can share a range.
/// Returns the combined field width when they can, `None` otherwise.
fn width_equiv(n: u64, wn: usize, m: u64, wm: usize) -> Option<usize> {
    if wn == wm {
        return Some(wn);
    }
    let npad = zero_padded(n, wn);
    let nmpad = zero_padded(n, wm);
    let mpad = zero_padded(m, wm);
    let mnpad = zero_padded(m, wn);
    if npad != nmpad && mpad != mnpad {
        None
    } else if npad != nmpad {
        Some(wn)
    } else {
        Some(wm)
    }
}

/// Slurm's `_zero_padded`: leading-zero count when `num` is printed at `width`.
fn zero_padded(num: u64, width: usize) -> usize {
    width.saturating_sub(digit_count(num))
}

fn digit_count(n: u64) -> usize {
    n.checked_ilog10().unwrap_or(0) as usize + 1
}

/// Natural ("version") string comparison: digit runs compare by numeric value;
/// on equal value the shorter run (less zero-padding) sorts first. Gives a
/// deterministic, human-friendly order (`rack2` before `rack10`); this is our
/// own ordering, not a port of Slurm's prefix sort (which uses plain `strcmp`).
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let a_start = i;
            let b_start = j;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let a_num = trim_leading_zeros(&a[a_start..i]);
            let b_num = trim_leading_zeros(&b[b_start..j]);
            let ord = a_num
                .len()
                .cmp(&b_num.len())
                .then_with(|| a_num.cmp(b_num))
                // Equal value: shorter run (less zero-padding) first.
                .then_with(|| (i - a_start).cmp(&(j - b_start)));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            match a[i].cmp(&b[j]) {
                Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
                ord => return ord,
            }
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

fn trim_leading_zeros(s: &[u8]) -> &[u8] {
    let k = s[..s.len().saturating_sub(1)]
        .iter()
        .take_while(|&&b| b == b'0')
        .count();
    &s[k..]
}

/// Split a hostname into (prefix, number, zero-padding width). Returns `None`
/// when the name has no trailing digit run or is entirely digits (no prefix).
///
/// Digits are ASCII, so scanning bytes keeps the split on a valid char boundary
/// even when the prefix ends in a multi-byte character.
fn split_name_number(name: &str) -> Option<(String, u64, usize)> {
    let bytes = name.as_bytes();
    let start = bytes
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |i| i + 1);
    if start == 0 || start == bytes.len() {
        return None;
    }
    let num_str = &name[start..];
    let num = num_str.parse::<u64>().ok()?;
    Some((name[..start].to_string(), num, num_str.len()))
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

/// First hostname of a single term (no top-level commas), or `None` when the
/// term expands to nothing (e.g. an empty string). Mirrors [`expand_single`]'s
/// parsing but only resolves the first element of the leading range.
fn first_single(pattern: &str) -> Result<Option<String>, HostlistError> {
    if pattern.is_empty() {
        return Ok(None);
    }

    let Some(bracket_start) = pattern.find('[') else {
        return Ok(Some(pattern.to_string()));
    };
    let bracket_end = pattern
        .find(']')
        .ok_or_else(|| HostlistError::InvalidPattern("unmatched [".into()))?;

    let prefix = &pattern[..bracket_start];
    let range_str = &pattern[bracket_start + 1..bracket_end];
    let suffix = &pattern[bracket_end + 1..];

    let first_part = range_str.split(',').next().unwrap_or_default();
    let first_value = if let Some(dash) = first_part.find('-') {
        let start_str = &first_part[..dash];
        let end_str = &first_part[dash + 1..];
        let width = start_str.len();
        let start: u64 = start_str
            .parse()
            .map_err(|_| HostlistError::InvalidRange(first_part.into()))?;
        let end: u64 = end_str
            .parse()
            .map_err(|_| HostlistError::InvalidRange(first_part.into()))?;
        if start > end {
            return Err(HostlistError::InvalidRange(format!("{} > {}", start, end)));
        }
        format!("{:0>width$}", start, width = width)
    } else {
        first_part.to_string()
    };

    let name = format!("{prefix}{first_value}{suffix}");
    if suffix.contains('[') {
        first_single(&name)
    } else {
        Ok(Some(name))
    }
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

    #[test]
    fn test_long_comma_separated_list() {
        let hosts = expand(
            "crsuse2-m2m-052,crsuse2-m2m-331,crsuse2-m2m-301,crsuse2-m2m-199,crsuse2-m2m-251",
        )
        .unwrap();
        assert_eq!(
            hosts,
            vec![
                "crsuse2-m2m-052",
                "crsuse2-m2m-331",
                "crsuse2-m2m-301",
                "crsuse2-m2m-199",
                "crsuse2-m2m-251",
            ]
        );
    }

    #[test]
    fn test_mixed_range_and_plain() {
        let hosts = expand("gpu[1-2],login01,cpu[01-03]").unwrap();
        assert_eq!(
            hosts,
            vec!["gpu1", "gpu2", "login01", "cpu01", "cpu02", "cpu03"]
        );
    }

    #[test]
    fn test_single_element_range() {
        let hosts = expand("node[5]").unwrap();
        assert_eq!(hosts, vec!["node5"]);
    }

    #[test]
    fn test_unmatched_bracket() {
        assert!(expand("node[1-3").is_err());
    }

    #[test]
    fn test_reversed_range() {
        assert!(expand("node[5-3]").is_err());
    }

    #[test]
    fn test_empty_string() {
        let hosts = expand("").unwrap();
        assert!(hosts.is_empty());
    }

    #[test]
    fn test_trailing_comma() {
        let hosts = expand("node1,node2,").unwrap();
        assert_eq!(hosts, vec!["node1", "node2"]);
    }

    #[test]
    fn test_leading_and_doubled_commas() {
        let hosts = expand(",node1,,node2").unwrap();
        assert_eq!(hosts, vec!["node1", "node2"]);
    }

    #[test]
    fn test_suffix_after_bracket() {
        let hosts = expand("rack[1-2]-node[1-2]").unwrap();
        assert_eq!(
            hosts,
            vec!["rack1-node1", "rack1-node2", "rack2-node1", "rack2-node2"]
        );
    }

    fn strings(hosts: &[&str]) -> Vec<String> {
        hosts.iter().map(|h| h.to_string()).collect()
    }

    #[test]
    fn test_compress_multiple_prefixes() {
        assert_eq!(
            compress(&strings(&["cpu001", "cpu002", "gpu001", "gpu002"])),
            "cpu[001-002],gpu[001-002]"
        );
    }

    #[test]
    fn test_compress_multiple_prefixes_input_order_independent() {
        assert_eq!(
            compress(&strings(&["gpu002", "cpu001", "gpu001", "cpu002"])),
            "cpu[001-002],gpu[001-002]"
        );
    }

    #[test]
    fn test_compress_multi_field_prefix() {
        // Only the trailing digit run is compressed; the prefix is preserved.
        assert_eq!(
            compress(&strings(&["crsuse2-m2m-028", "crsuse2-m2m-219"])),
            "crsuse2-m2m-[028,219]"
        );
    }

    #[test]
    fn test_compress_bare_and_numeric_same_spelling() {
        // A bare `node` must not be swallowed by `node1`'s prefix.
        assert_eq!(compress(&strings(&["node", "node1"])), "node,node1");
        assert_eq!(compress(&strings(&["node1", "node"])), "node,node1");
    }

    #[test]
    fn test_compress_bare_names_sorted() {
        assert_eq!(compress(&strings(&["master", "login"])), "login,master");
    }

    #[test]
    fn test_compress_mixed_zero_pad_width() {
        // Incompatible paddings share the prefix bracket but stay separate
        // terms, so the result round-trips (Slurm's `node[9,010-011]`).
        assert_eq!(
            compress(&strings(&["node9", "node010", "node011"])),
            "node[9,010-011]"
        );
    }

    #[test]
    fn test_compress_prefixes_natural_sorted() {
        // Natural order: rack2 before rack10 (not lexicographic rack10 first).
        assert_eq!(
            compress(&strings(&["rack10-n1", "rack2-n1", "rack1-n1"])),
            "rack1-n1,rack2-n1,rack10-n1"
        );
    }

    #[test]
    fn test_compress_unpadded_across_digit_lengths() {
        assert_eq!(
            compress(&strings(&["node8", "node9", "node10"])),
            "node[8-10]"
        );
    }

    #[test]
    fn test_compress_dedup() {
        assert_eq!(compress(&strings(&["node001", "node001"])), "node001");
        assert_eq!(
            compress(&strings(&["node001", "node002", "node001"])),
            "node[001-002]"
        );
    }

    #[test]
    fn test_compress_single_host_verbatim() {
        assert_eq!(compress(&strings(&["gpu001"])), "gpu001");
        assert_eq!(compress(&strings(&["master"])), "master");
    }

    #[test]
    fn test_compress_empty() {
        assert_eq!(compress(&[]), "");
    }

    #[test]
    fn test_compress_multibyte_prefix_no_panic() {
        // Prefix ending in a multi-byte char must not panic on the digit split.
        assert_eq!(compress(&strings(&["café1", "café2"])), "café[1-2]");
    }

    #[test]
    fn test_compress_all_digit_names_are_bare() {
        // Names that are entirely digits have no prefix; kept verbatim, deduped.
        assert_eq!(compress(&strings(&["12345"])), "12345");
        assert_eq!(compress(&strings(&["7", "7"])), "7");
    }

    #[test]
    fn test_expand_first_range() {
        assert_eq!(
            expand_first("node[001-002]").unwrap().as_deref(),
            Some("node001")
        );
    }

    #[test]
    fn test_expand_first_gap() {
        assert_eq!(
            expand_first("node[001,003]").unwrap().as_deref(),
            Some("node001")
        );
    }

    #[test]
    fn test_expand_first_multi_prefix() {
        assert_eq!(
            expand_first("gpu[001-004],cpu[001-002]")
                .unwrap()
                .as_deref(),
            Some("gpu001")
        );
    }

    #[test]
    fn test_expand_first_mixed_padding() {
        assert_eq!(
            expand_first("node[9,010-011]").unwrap().as_deref(),
            Some("node9")
        );
    }

    #[test]
    fn test_expand_first_plain_list() {
        assert_eq!(
            expand_first("node001,node002").unwrap().as_deref(),
            Some("node001")
        );
    }

    #[test]
    fn test_expand_first_single_host() {
        assert_eq!(expand_first("node007").unwrap().as_deref(), Some("node007"));
    }

    #[test]
    fn test_expand_first_empty_is_none() {
        assert_eq!(expand_first("").unwrap(), None);
        assert_eq!(expand_first(",,").unwrap(), None);
    }

    #[test]
    fn test_expand_first_skips_leading_empty_terms() {
        assert_eq!(
            expand_first(",node1,node2").unwrap().as_deref(),
            Some("node1")
        );
    }

    #[test]
    fn test_expand_first_suffix_bracket() {
        assert_eq!(
            expand_first("rack[1-2]-node[3-4]").unwrap().as_deref(),
            Some("rack1-node3")
        );
    }

    #[test]
    fn test_expand_first_unmatched_bracket_errors() {
        assert!(expand_first("node[1-2").is_err());
    }

    #[test]
    fn test_expand_first_matches_expand() {
        for pattern in [
            "node[001-003,005,010-012]",
            "rack[1-2]-node[1-2]",
            "gpu[01-04],cpu[01-02]",
            "node9,node010,node011",
            "login01",
        ] {
            assert_eq!(
                expand_first(pattern).unwrap(),
                expand(pattern).unwrap().into_iter().next(),
                "expand_first disagreed with expand for {pattern}"
            );
        }
    }

    #[test]
    fn test_compress_roundtrip_mixed() {
        // Any set survives expand(compress(x)) once both sides are sorted.
        let hosts = strings(&[
            "node9", "node010", "node011", "gpu001", "gpu003", "master", "cpu10", "cpu8",
        ]);
        let mut expected = hosts.clone();
        expected.sort();
        expected.dedup();

        let mut round_tripped = expand(&compress(&hosts)).unwrap();
        round_tripped.sort();

        assert_eq!(round_tripped, expected);
    }
}
