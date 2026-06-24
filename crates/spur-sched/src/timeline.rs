// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use chrono::{DateTime, Duration, Utc};
use spur_core::resource::{ResourceAllocations, ResourceSet};

/// Per-node resource timeline for backfill scheduling.
///
/// Tracks when resources become available on a node by maintaining
/// a sorted list of allocation intervals.
#[derive(Debug, Clone)]
pub struct NodeTimeline {
    pub node_name: String,
    pub total: ResourceSet,
    pub intervals: Vec<Interval>,
}

/// A time interval during which resources are allocated.
#[derive(Debug, Clone)]
pub struct Interval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub resources: ResourceAllocations,
}

/// Incremental sweep over sorted interval start/end events.
///
/// Maintains the sum of resources active at the current sweep time `t`
/// (half-open intervals: `start <= t < end`).
struct AllocationSweep<'a> {
    intervals: &'a [Interval],
    used: ResourceAllocations,
    next_start: usize,
    ending: BinaryHeap<Reverse<(DateTime<Utc>, usize)>>,
}

impl<'a> AllocationSweep<'a> {
    fn new(intervals: &'a [Interval], time: DateTime<Utc>) -> Self {
        let mut sweep = Self {
            intervals,
            used: ResourceAllocations::default(),
            next_start: 0,
            ending: BinaryHeap::new(),
        };
        sweep.advance_to(time);
        sweep
    }

    fn advance_to(&mut self, time: DateTime<Utc>) {
        while self.next_start < self.intervals.len()
            && self.intervals[self.next_start].start <= time
        {
            let idx = self.next_start;
            let interval = &self.intervals[idx];
            self.used.add(&interval.resources);
            self.ending.push(Reverse((interval.end, idx)));
            self.next_start += 1;
        }

        while let Some(Reverse((end, idx))) = self.ending.peek().copied() {
            if end <= time {
                self.ending.pop();
                self.used.subtract(&self.intervals[idx].resources);
            } else {
                break;
            }
        }
    }

    fn used(&self) -> &ResourceAllocations {
        &self.used
    }
}

impl NodeTimeline {
    pub fn new(node_name: String, total: ResourceSet) -> Self {
        Self {
            node_name,
            total,
            intervals: Vec::new(),
        }
    }

    /// Index one past intervals with `start <= time` (`intervals` sorted by `start`).
    fn active_prefix_end(&self, time: DateTime<Utc>) -> usize {
        self.intervals.partition_point(|i| i.start <= time)
    }

    /// Index one past intervals with `start < window_end` (`intervals` sorted by `start`).
    fn overlap_prefix_end(&self, window_end: DateTime<Utc>) -> usize {
        self.intervals.partition_point(|i| i.start < window_end)
    }

    pub fn accumulated_at(&self, time: DateTime<Utc>) -> ResourceAllocations {
        let mut used = ResourceAllocations::default();
        for interval in &self.intervals[..self.active_prefix_end(time)] {
            if interval.end > time {
                used.add(&interval.resources);
            }
        }
        used
    }

    /// Whether the request can be satisfied at a specific time.
    pub fn can_satisfy_at(&self, time: DateTime<Utc>, request: &ResourceSet) -> bool {
        let used = self.accumulated_at(time);
        self.total.can_satisfy_with_allocated(&used, request)
    }

    /// Find the earliest time at which `request` resources are available
    /// for `duration` contiguous time.
    pub fn earliest_start(
        &mut self,
        request: &ResourceSet,
        duration: Duration,
        after: DateTime<Utc>,
    ) -> DateTime<Utc> {
        self.gc(after);

        let mut candidate = after;
        let max_check = after + Duration::days(365);
        let mut sweep = AllocationSweep::new(&self.intervals, after);

        loop {
            if candidate > max_check {
                return max_check;
            }

            sweep.advance_to(candidate);

            if self.total.can_satisfy_with_allocated(sweep.used(), request) {
                let window_end = candidate + duration;
                let mut ok = true;
                for interval in &self.intervals[..self.overlap_prefix_end(window_end)] {
                    if interval.end > candidate {
                        let mut used = ResourceAllocations::default();
                        used.add(&interval.resources);
                        if !self.total.can_satisfy_with_allocated(&used, request) {
                            candidate = interval.end;
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    return candidate;
                }
            } else {
                let next_end = self
                    .intervals
                    .iter()
                    .filter(|i| i.end > candidate)
                    .map(|i| i.end)
                    .min();
                match next_end {
                    Some(t) => candidate = t,
                    None => return candidate,
                }
            }
        }
    }

    /// Reserve resources on this node for a time window.
    pub fn reserve(
        &mut self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        resources: ResourceAllocations,
    ) {
        self.intervals.push(Interval {
            start,
            end,
            resources,
        });
        self.intervals.sort_by_key(|i| i.start);
    }

    /// Remove a reservation (when a job completes or is cancelled).
    pub fn release(&mut self, start: DateTime<Utc>, end: DateTime<Utc>) {
        self.intervals
            .retain(|i| !(i.start == start && i.end == end));
    }

    /// Drop intervals that end at or before `horizon`.
    ///
    /// Such intervals cannot affect any query at times `t` where `t >= horizon`
    /// (half-open active range: `start <= t < end`).
    pub fn gc(&mut self, horizon: DateTime<Utc>) {
        self.intervals.retain(|i| i.end > horizon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use spur_core::resource::{AllocatedDevice, GpuResource};

    fn make_timeline() -> NodeTimeline {
        NodeTimeline::new(
            "node001".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        )
    }

    fn make_gpu_timeline(num_gpus: usize) -> NodeTimeline {
        let gpus = (0..num_gpus)
            .map(|i| GpuResource {
                device_id: i as u32,
                gpu_type: "mi355x".into(),
                memory_mb: 192_000,
                peer_gpus: vec![],
                link_type: spur_core::resource::GpuLinkType::PCIe,
            })
            .collect();
        NodeTimeline::new(
            "gpu-node".into(),
            ResourceSet {
                cpus: 192,
                memory_mb: 2_000_000,
                gpus,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_empty_timeline() {
        let tl = make_timeline();
        let now = Utc::now();
        let req = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            ..Default::default()
        };
        assert!(tl.can_satisfy_at(now, &req));
    }

    #[test]
    fn test_reservation() {
        let mut tl = make_timeline();
        let now = Utc::now();
        tl.reserve(
            now,
            now + Duration::hours(4),
            ResourceAllocations::with_scalar(32, 128_000),
        );

        let req_full = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            ..Default::default()
        };
        assert!(!tl.can_satisfy_at(now + Duration::hours(1), &req_full));

        let req_partial = ResourceSet {
            cpus: 32,
            memory_mb: 128_000,
            ..Default::default()
        };
        assert!(tl.can_satisfy_at(now + Duration::hours(1), &req_partial));
        assert!(tl.can_satisfy_at(now + Duration::hours(5), &req_full));
    }

    #[test]
    fn test_earliest_start() {
        let mut tl = make_timeline();
        let now = Utc::now();

        tl.reserve(
            now,
            now + Duration::hours(4),
            ResourceAllocations::with_scalar(48, 0),
        );

        let req = ResourceSet {
            cpus: 32,
            memory_mb: 0,
            ..Default::default()
        };
        let start = tl.earliest_start(&req, Duration::hours(2), now);
        assert!(start >= now + Duration::hours(4));
    }

    #[test]
    fn test_gpu_reservation_blocks_scheduling() {
        let mut tl = make_gpu_timeline(8);
        let now = Utc::now();

        let mut alloc = ResourceAllocations::with_scalar(8, 0);
        alloc.devices.insert(
            "gpu".into(),
            (0u32..8).map(AllocatedDevice::injectable).collect(),
        );
        tl.reserve(now, now + Duration::hours(4), alloc);

        let req = ResourceSet {
            cpus: 4,
            memory_mb: 0,
            gpus: (0..4)
                .map(|i| GpuResource {
                    device_id: i as u32,
                    gpu_type: "mi355x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: spur_core::resource::GpuLinkType::PCIe,
                })
                .collect(),
            ..Default::default()
        };
        assert!(!tl.can_satisfy_at(now + Duration::minutes(1), &req));
    }

    #[test]
    fn test_gpu_partial_reservation_allows_remaining() {
        let mut tl = make_gpu_timeline(8);
        let now = Utc::now();

        let mut alloc = ResourceAllocations::with_scalar(4, 0);
        alloc.devices.insert(
            "gpu".into(),
            (0u32..4).map(AllocatedDevice::injectable).collect(),
        );
        tl.reserve(now, now + Duration::hours(4), alloc);

        let req = ResourceSet {
            cpus: 4,
            memory_mb: 0,
            gpus: (0..4)
                .map(|i| GpuResource {
                    device_id: i as u32,
                    gpu_type: "mi355x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: spur_core::resource::GpuLinkType::PCIe,
                })
                .collect(),
            ..Default::default()
        };
        assert!(tl.can_satisfy_at(now + Duration::minutes(1), &req));
    }

    #[test]
    fn test_accumulated_at_many_intervals() {
        let mut tl = make_timeline();
        let base = Utc::now();

        for i in 0..50 {
            let start = base + Duration::hours(i);
            tl.reserve(
                start,
                start + Duration::hours(2),
                ResourceAllocations::with_scalar(4, 0),
            );
        }

        let query = base + Duration::hours(25) + Duration::minutes(30);
        let used = tl.accumulated_at(query);

        let mut brute = ResourceAllocations::default();
        for interval in &tl.intervals {
            if interval.start <= query && query < interval.end {
                brute.add(&interval.resources);
            }
        }

        assert_eq!(used.cpus, brute.cpus);
        assert_eq!(used.memory_mb, brute.memory_mb);
    }

    #[test]
    fn test_earliest_start_many_intervals() {
        let mut tl = make_timeline();
        let base = Utc::now();

        tl.reserve(
            base,
            base + Duration::hours(8),
            ResourceAllocations::with_scalar(48, 0),
        );
        for i in 1..40 {
            let start = base + Duration::hours(i * 10);
            tl.reserve(
                start,
                start + Duration::hours(4),
                ResourceAllocations::with_scalar(8, 0),
            );
        }

        let req = ResourceSet {
            cpus: 32,
            memory_mb: 0,
            ..Default::default()
        };
        let start = tl.earliest_start(&req, Duration::hours(2), base);
        assert!(start >= base + Duration::hours(8));
    }

    fn brute_earliest_start(
        tl: &NodeTimeline,
        request: &ResourceSet,
        duration: Duration,
        after: DateTime<Utc>,
    ) -> DateTime<Utc> {
        let mut candidate = after;
        let max_check = after + Duration::days(365);

        loop {
            if candidate > max_check {
                return max_check;
            }

            if tl.can_satisfy_at(candidate, request) {
                let window_end = candidate + duration;
                let mut ok = true;
                for interval in &tl.intervals[..tl.overlap_prefix_end(window_end)] {
                    if interval.end > candidate {
                        let mut used = ResourceAllocations::default();
                        used.add(&interval.resources);
                        if !tl.total.can_satisfy_with_allocated(&used, request) {
                            candidate = interval.end;
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    return candidate;
                }
            } else {
                let next_end = tl
                    .intervals
                    .iter()
                    .filter(|i| i.end > candidate)
                    .map(|i| i.end)
                    .min();
                match next_end {
                    Some(t) => candidate = t,
                    None => return candidate,
                }
            }
        }
    }

    #[test]
    fn test_sweep_matches_accumulated_at() {
        let mut tl = make_timeline();
        let base = Utc::now();

        tl.reserve(
            base,
            base + Duration::hours(6),
            ResourceAllocations::with_scalar(20, 0),
        );
        tl.reserve(
            base + Duration::hours(2),
            base + Duration::hours(8),
            ResourceAllocations::with_scalar(16, 0),
        );
        tl.reserve(
            base + Duration::hours(2),
            base + Duration::hours(4),
            ResourceAllocations::with_scalar(8, 0),
        );
        tl.reserve(
            base + Duration::hours(10),
            base + Duration::hours(12),
            ResourceAllocations::with_scalar(32, 0),
        );

        let mut event_times = vec![
            base,
            base + Duration::hours(2),
            base + Duration::hours(4),
            base + Duration::hours(6),
            base + Duration::hours(8),
            base + Duration::hours(10),
            base + Duration::hours(12),
        ];
        event_times.sort();
        event_times.dedup();

        let mut sweep = AllocationSweep::new(&tl.intervals, base);
        for time in event_times {
            sweep.advance_to(time);
            let expected = tl.accumulated_at(time);
            assert_eq!(
                sweep.used().cpus,
                expected.cpus,
                "cpu mismatch at {time}"
            );
            assert_eq!(
                sweep.used().memory_mb,
                expected.memory_mb,
                "memory mismatch at {time}"
            );
            assert_eq!(
                sweep.used().total_device_count("gpu"),
                expected.total_device_count("gpu"),
                "gpu mismatch at {time}"
            );
        }
    }

    #[test]
    fn test_earliest_start_overlapping_intervals() {
        let mut tl = make_timeline();
        let base = Utc::now();

        tl.reserve(
            base,
            base + Duration::hours(3),
            ResourceAllocations::with_scalar(40, 0),
        );
        tl.reserve(
            base + Duration::hours(1),
            base + Duration::hours(5),
            ResourceAllocations::with_scalar(30, 0),
        );

        let req = ResourceSet {
            cpus: 32,
            ..Default::default()
        };
        let duration = Duration::hours(1);

        let got = tl.earliest_start(&req, duration, base);
        let want = brute_earliest_start(&tl, &req, duration, base);
        assert_eq!(got, want);
    }

    #[test]
    fn test_earliest_start_same_start_time() {
        let mut tl = make_timeline();
        let base = Utc::now();

        for cpus in [24_u32, 16, 8] {
            tl.reserve(
                base,
                base + Duration::hours(2),
                ResourceAllocations::with_scalar(cpus, 0),
            );
        }

        let req = ResourceSet {
            cpus: 8,
            ..Default::default()
        };
        let duration = Duration::minutes(30);

        let got = tl.earliest_start(&req, duration, base);
        let want = brute_earliest_start(&tl, &req, duration, base);
        assert_eq!(got, want);
    }

    #[test]
    fn test_earliest_start_gpu_sweep() {
        let mut tl = make_gpu_timeline(8);
        let base = Utc::now();

        let mut alloc_a = ResourceAllocations::with_scalar(4, 0);
        alloc_a.devices.insert(
            "gpu".into(),
            (0u32..4).map(AllocatedDevice::injectable).collect(),
        );
        tl.reserve(base, base + Duration::hours(3), alloc_a);

        let mut alloc_b = ResourceAllocations::with_scalar(4, 0);
        alloc_b.devices.insert(
            "gpu".into(),
            (4u32..6).map(AllocatedDevice::injectable).collect(),
        );
        tl.reserve(base + Duration::hours(1), base + Duration::hours(4), alloc_b);

        let req = ResourceSet {
            cpus: 4,
            memory_mb: 0,
            gpus: (0..2)
                .map(|i| GpuResource {
                    device_id: i as u32,
                    gpu_type: "mi355x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: spur_core::resource::GpuLinkType::PCIe,
                })
                .collect(),
            ..Default::default()
        };
        let duration = Duration::hours(1);

        let got = tl.earliest_start(&req, duration, base);
        let want = brute_earliest_start(&tl, &req, duration, base);
        assert_eq!(got, want);
    }

    #[test]
    fn test_earliest_start_prunes_expired_intervals() {
        let mut tl = make_timeline();
        let base = Utc::now();

        tl.reserve(
            base - Duration::hours(3),
            base - Duration::hours(1),
            ResourceAllocations::with_scalar(64, 0),
        );
        tl.reserve(
            base,
            base + Duration::hours(4),
            ResourceAllocations::with_scalar(48, 0),
        );
        assert_eq!(tl.intervals.len(), 2);

        let req = ResourceSet {
            cpus: 32,
            ..Default::default()
        };
        let start = tl.earliest_start(&req, Duration::hours(1), base);
        assert!(start >= base + Duration::hours(4));
        assert_eq!(tl.intervals.len(), 1);
        assert!(tl.intervals[0].end > base);
    }

    #[test]
    fn test_gc_keeps_future_and_active_intervals() {
        let mut tl = make_timeline();
        let base = Utc::now();

        tl.reserve(
            base - Duration::hours(1),
            base + Duration::hours(1),
            ResourceAllocations::with_scalar(16, 0),
        );
        tl.reserve(
            base + Duration::hours(2),
            base + Duration::hours(4),
            ResourceAllocations::with_scalar(8, 0),
        );

        tl.gc(base);
        assert_eq!(tl.intervals.len(), 2);

        let used = tl.accumulated_at(base);
        assert_eq!(used.cpus, 16);
    }

    #[test]
    fn test_earliest_start_brute_many_intervals() {
        let mut tl = make_timeline();
        let base = Utc::now();

        for i in 0..30 {
            let start = base + Duration::minutes(i * 17);
            let cpus = 8 + (i % 5) as u32 * 4;
            tl.reserve(
                start,
                start + Duration::hours(2),
                ResourceAllocations::with_scalar(cpus, 0),
            );
        }

        let req = ResourceSet {
            cpus: 24,
            memory_mb: 0,
            ..Default::default()
        };
        let duration = Duration::minutes(45);

        let got = tl.earliest_start(&req, duration, base);
        let want = brute_earliest_start(&tl, &req, duration, base);
        assert_eq!(got, want);
    }
}
