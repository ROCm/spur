use chrono::{DateTime, Duration, Utc};
use spur_core::resource::ResourceSet;

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
    pub resources: ResourceSet,
}

impl NodeTimeline {
    pub fn new(node_name: String, total: ResourceSet) -> Self {
        Self {
            node_name,
            total,
            intervals: Vec::new(),
        }
    }

    /// Get available resources at a specific time.
    pub fn available_at(&self, time: DateTime<Utc>) -> ResourceSet {
        let mut used = ResourceSet::default();
        for interval in &self.intervals {
            if interval.start <= time && time < interval.end {
                used.cpus += interval.resources.cpus;
                used.memory_mb += interval.resources.memory_mb;
                // Accumulate GPU allocations so subtract() can remove them by device_id.
                for gpu in &interval.resources.gpus {
                    if !used.gpus.iter().any(|g| g.device_id == gpu.device_id) {
                        used.gpus.push(gpu.clone());
                    }
                }
            }
        }
        self.total.subtract(&used)
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
        let max_check = after + Duration::days(365); // Safety bound

        loop {
            if candidate > max_check {
                return max_check;
            }

            let available = self.available_at(candidate);
            if available.can_satisfy(request) {
                // Check that resources remain available for the full duration
                let end = candidate + duration;
                let mut ok = true;
                for interval in &self.intervals {
                    if interval.start < end && interval.end > candidate {
                        // This interval overlaps our proposed window
                        let mut total_used = ResourceSet::default();
                        total_used.cpus += interval.resources.cpus;
                        total_used.memory_mb += interval.resources.memory_mb;
                        for gpu in &interval.resources.gpus {
                            if !total_used.gpus.iter().any(|g| g.device_id == gpu.device_id) {
                                total_used.gpus.push(gpu.clone());
                            }
                        }
                        let avail = self.total.subtract(&total_used);
                        if !avail.can_satisfy(request) {
                            // Move candidate past this interval
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
                // Find the next interval end time to try
                let next_end = self
                    .intervals
                    .iter()
                    .filter(|i| i.end > candidate)
                    .map(|i| i.end)
                    .min();
                match next_end {
                    Some(t) => candidate = t,
                    None => return candidate, // No more intervals, resources should be free
                }
            }
        }
    }

    /// Reserve resources on this node for a time window.
    pub fn reserve(&mut self, start: DateTime<Utc>, end: DateTime<Utc>, resources: ResourceSet) {
        self.intervals.push(Interval {
            start,
            end,
            resources,
        });
        // Keep sorted by start time
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
    use spur_core::resource::GpuResource;

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
        let avail = tl.available_at(now);
        assert_eq!(avail.cpus, 64);
        assert_eq!(avail.memory_mb, 256_000);
    }

    #[test]
    fn test_reservation() {
        let mut tl = make_timeline();
        let now = Utc::now();
        tl.reserve(
            now,
            now + Duration::hours(4),
            ResourceSet {
                cpus: 32,
                memory_mb: 128_000,
                ..Default::default()
            },
        );

        let avail = tl.available_at(now + Duration::hours(1));
        assert_eq!(avail.cpus, 32);
        assert_eq!(avail.memory_mb, 128_000);

        // After reservation ends
        let avail = tl.available_at(now + Duration::hours(5));
        assert_eq!(avail.cpus, 64);
    }

    #[test]
    fn test_earliest_start() {
        let mut tl = make_timeline();
        let now = Utc::now();

        // Reserve 48 CPUs for 4 hours
        tl.reserve(
            now,
            now + Duration::hours(4),
            ResourceSet {
                cpus: 48,
                memory_mb: 0,
                ..Default::default()
            },
        );

        // Request 32 CPUs for 2 hours - should fit now (64-48=16 < 32, so must wait)
        let req = ResourceSet {
            cpus: 32,
            memory_mb: 0,
            ..Default::default()
        };
        let start = tl.earliest_start(&req, Duration::hours(2), now);
        // Should start after the reservation ends
        assert!(start >= now + Duration::hours(4));
    }

    #[test]
    fn test_gpu_reservation_blocks_scheduling() {
        let mut tl = make_gpu_timeline(8);
        let now = Utc::now();

        // Reserve all 8 GPUs (simulating job 13 with 8 GPUs on gpu-2)
        let all_gpus: Vec<GpuResource> = (0..8)
            .map(|i| GpuResource {
                device_id: i as u32,
                gpu_type: "mi355x".into(),
                memory_mb: 192_000,
                peer_gpus: vec![],
                link_type: spur_core::resource::GpuLinkType::PCIe,
            })
            .collect();
        tl.reserve(
            now,
            now + Duration::hours(4),
            ResourceSet {
                cpus: 8,
                memory_mb: 0,
                gpus: all_gpus,
                ..Default::default()
            },
        );

        // Now try to schedule a job needing 4 GPUs - should see 0 available
        let avail = tl.available_at(now + Duration::minutes(1));
        assert_eq!(avail.gpus.len(), 0, "all GPUs should be allocated");

        // Request 4 GPUs - should not be satisfiable now
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
        assert!(
            !avail.can_satisfy(&req),
            "should not schedule when GPUs are full"
        );
    }

    #[test]
    fn test_gpu_partial_reservation_allows_remaining() {
        let mut tl = make_gpu_timeline(8);
        let now = Utc::now();

        // Reserve 4 GPUs (devices 0-3)
        let half_gpus: Vec<GpuResource> = (0..4)
            .map(|i| GpuResource {
                device_id: i as u32,
                gpu_type: "mi355x".into(),
                memory_mb: 192_000,
                peer_gpus: vec![],
                link_type: spur_core::resource::GpuLinkType::PCIe,
            })
            .collect();
        tl.reserve(
            now,
            now + Duration::hours(4),
            ResourceSet {
                cpus: 4,
                memory_mb: 0,
                gpus: half_gpus,
                ..Default::default()
            },
        );

        let avail = tl.available_at(now + Duration::minutes(1));
        assert_eq!(avail.gpus.len(), 4, "4 GPUs should still be available");
    }
}
