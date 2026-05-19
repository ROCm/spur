// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T28b: Job array expansion tests.
//!
//! Tests array spec parsing — complement to t28_arrays which tests
//! array job fields on Job structs.

#[cfg(test)]
mod tests {
    use spur_core::array::*;

    #[test]
    fn t28b_1_simple_range() {
        let spec = parse_array_spec("0-9").unwrap();
        assert_eq!(spec.task_ids, (0..=9).collect::<Vec<_>>());
        assert_eq!(spec.max_concurrent, 0);
    }

    #[test]
    fn t28b_2_range_with_limit() {
        let spec = parse_array_spec("0-99%10").unwrap();
        assert_eq!(spec.task_ids.len(), 100);
        assert_eq!(spec.max_concurrent, 10);
    }

    #[test]
    fn t28b_3_specific_ids() {
        let spec = parse_array_spec("1,3,5,7").unwrap();
        assert_eq!(spec.task_ids, vec![1, 3, 5, 7]);
    }

    #[test]
    fn t28b_4_step() {
        let spec = parse_array_spec("0-10:2").unwrap();
        assert_eq!(spec.task_ids, vec![0, 2, 4, 6, 8, 10]);
    }

    #[test]
    fn t28b_5_mixed_ranges() {
        let spec = parse_array_spec("1-5,10-15").unwrap();
        assert_eq!(spec.task_ids.len(), 11);
        assert_eq!(spec.task_ids[0], 1);
        assert_eq!(spec.task_ids[5], 10);
    }

    #[test]
    fn t28b_6_single_id() {
        let spec = parse_array_spec("42").unwrap();
        assert_eq!(spec.task_ids, vec![42]);
    }

    #[test]
    fn t28b_7_large_array() {
        let spec = parse_array_spec("0-9999").unwrap();
        assert_eq!(spec.task_ids.len(), 10000);
    }

    #[test]
    fn t28b_8_step_with_limit() {
        let spec = parse_array_spec("0-100:5%10").unwrap();
        assert_eq!(spec.task_ids, (0..=100).step_by(5).collect::<Vec<_>>());
        assert_eq!(spec.max_concurrent, 10);
    }

    #[test]
    fn t28b_9_empty_fails() {
        assert!(parse_array_spec("").is_err());
    }

    #[test]
    fn t28b_10_reversed_range_fails() {
        assert!(parse_array_spec("10-5").is_err());
    }

    #[test]
    fn t28b_11_zero_step_fails() {
        assert!(parse_array_spec("0-10:0").is_err());
    }

    #[test]
    fn t28b_12_dedup() {
        let spec = parse_array_spec("1,1,2,2,3").unwrap();
        assert_eq!(spec.task_ids, vec![1, 2, 3]);
    }
}
