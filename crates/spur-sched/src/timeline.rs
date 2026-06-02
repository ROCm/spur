// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

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

impl NodeTimeline {
    pub fn new(node_name: String, total: ResourceSet) -> Self {
        Self {
            node_name,
            total,
            intervals: Vec::new(),
        }
    }

    pub fn accumulated_at(&self, time: DateTime<Utc>) -> ResourceAllocations {
        let mut used = ResourceAllocations::default();
        for interval in &self.intervals {
            if interval.start <= time && time < interval.end {
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
        &self,
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

            if self.can_satisfy_at(candidate, request) {
                let end = candidate + duration;
                let mut ok = true;
                for interval in &self.intervals {
                    if interval.start < end && interval.end > candidate {
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

    /// Clean up expired intervals.
    pub fn gc(&mut self, now: DateTime<Utc>) {
        self.intervals.retain(|i| i.end > now);
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
}
