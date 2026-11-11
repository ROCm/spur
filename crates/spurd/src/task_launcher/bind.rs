// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use spur_core::step::CpuBind;

/// The CPUs one task is pinned to, matching the `taskset` prefix the bash
/// wrapper generated (including its error messages, which users see on
/// stderr). `None` means the task inherits the step's affinity.
pub fn task_affinity(bind: &CpuBind, global_rank: u32) -> Result<Option<Vec<usize>>, String> {
    let rank = global_rank as usize;
    match bind {
        CpuBind::Rank => Ok(Some(vec![rank])),
        CpuBind::Map(list) => {
            let entries = split_entries(list);
            if entries.is_empty() {
                return Ok(None);
            }
            let entry = entries.get(rank).ok_or_else(|| {
                format!(
                    "map_cpu: rank {rank} exceeds CPU map (len {})",
                    entries.len()
                )
            })?;
            parse_cpu_list(entry).map(Some)
        }
        CpuBind::Mask(masks) => {
            let entries = split_entries(masks);
            let mask = match entries.as_slice() {
                [] => return Ok(None),
                [only] => only,
                many => many.get(rank).ok_or_else(|| {
                    format!(
                        "mask_cpu: rank {rank} exceeds CPU mask list (len {})",
                        many.len()
                    )
                })?,
            };
            parse_cpu_mask(mask).map(Some)
        }
        CpuBind::None | CpuBind::Cores | CpuBind::Threads | CpuBind::Sockets | CpuBind::Ldoms => {
            Ok(None)
        }
    }
}

fn split_entries(list: &str) -> Vec<&str> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// `taskset -c` list syntax: `N`, `N-M`, or `N-M:stride`, comma separated.
fn parse_cpu_list(list: &str) -> Result<Vec<usize>, String> {
    let fail = || format!("taskset: failed to parse CPU list: {list}");
    let mut cpus = Vec::new();
    for item in list.split(',') {
        let (range, stride) = match item.split_once(':') {
            Some((range, stride)) => (range, stride.parse::<usize>().map_err(|_| fail())?),
            None => (item, 1),
        };
        let (first, last) = match range.split_once('-') {
            Some((first, last)) => (first, last),
            None => (range, range),
        };
        let first = first.parse::<usize>().map_err(|_| fail())?;
        let last = last.parse::<usize>().map_err(|_| fail())?;
        if stride == 0 || first > last {
            return Err(fail());
        }
        cpus.extend((first..=last).step_by(stride));
    }
    Ok(cpus)
}

/// `taskset` mask syntax: hexadecimal, with or without `0x`.
fn parse_cpu_mask(mask: &str) -> Result<Vec<usize>, String> {
    let fail = || format!("taskset: failed to parse CPU mask: {mask}");
    let digits = mask
        .strip_prefix("0x")
        .or_else(|| mask.strip_prefix("0X"))
        .unwrap_or(mask);
    if digits.is_empty() {
        return Err(fail());
    }
    let mut cpus = Vec::new();
    for (nibble, c) in digits.chars().rev().enumerate() {
        let value = c.to_digit(16).ok_or_else(fail)?;
        cpus.extend(
            (0..4)
                .filter(|bit| value & (1 << bit) != 0)
                .map(|bit| nibble * 4 + bit),
        );
    }
    Ok(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_binds_each_task_to_its_rank() {
        assert_eq!(task_affinity(&CpuBind::Rank, 3), Ok(Some(vec![3])));
    }

    #[test]
    fn map_cpu_indexes_the_list_by_rank() {
        let bind = CpuBind::Map("0,5,7".into());
        assert_eq!(task_affinity(&bind, 1), Ok(Some(vec![5])));
    }

    #[test]
    fn map_cpu_entries_accept_taskset_list_syntax() {
        let bind = CpuBind::Map("2-4,8-12:2".into());
        assert_eq!(task_affinity(&bind, 0), Ok(Some(vec![2, 3, 4])));
        assert_eq!(task_affinity(&bind, 1), Ok(Some(vec![8, 10, 12])));
    }

    #[test]
    fn map_cpu_past_the_list_fails_like_the_wrapper() {
        let bind = CpuBind::Map("0,5,7".into());
        assert_eq!(
            task_affinity(&bind, 3),
            Err("map_cpu: rank 3 exceeds CPU map (len 3)".to_string())
        );
    }

    #[test]
    fn an_empty_map_binds_nothing() {
        assert_eq!(task_affinity(&CpuBind::Map(String::new()), 0), Ok(None));
    }

    #[test]
    fn an_unparsable_map_entry_fails_like_taskset() {
        let bind = CpuBind::Map("x".into());
        assert_eq!(
            task_affinity(&bind, 0),
            Err("taskset: failed to parse CPU list: x".to_string())
        );
    }

    #[test]
    fn one_mask_applies_to_every_rank() {
        let bind = CpuBind::Mask("0x3".into());
        assert_eq!(task_affinity(&bind, 0), Ok(Some(vec![0, 1])));
        assert_eq!(task_affinity(&bind, 9), Ok(Some(vec![0, 1])));
    }

    #[test]
    fn a_mask_without_0x_is_still_hex() {
        assert_eq!(
            task_affinity(&CpuBind::Mask("f".into()), 0),
            Ok(Some(vec![0, 1, 2, 3]))
        );
    }

    #[test]
    fn a_mask_list_indexes_by_rank() {
        let bind = CpuBind::Mask("0x1,0x2".into());
        assert_eq!(task_affinity(&bind, 1), Ok(Some(vec![1])));
        assert_eq!(
            task_affinity(&bind, 2),
            Err("mask_cpu: rank 2 exceeds CPU mask list (len 2)".to_string())
        );
    }

    #[test]
    fn an_unparsable_mask_fails_like_taskset() {
        assert_eq!(
            task_affinity(&CpuBind::Mask("zz".into()), 0),
            Err("taskset: failed to parse CPU mask: zz".to_string())
        );
    }

    #[test]
    fn topology_and_no_bind_leave_affinity_alone() {
        for bind in [
            CpuBind::None,
            CpuBind::Cores,
            CpuBind::Threads,
            CpuBind::Sockets,
            CpuBind::Ldoms,
        ] {
            assert_eq!(task_affinity(&bind, 0), Ok(None));
        }
    }
}
