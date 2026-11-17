// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The named input of `ClusterManager::update_partition`, and the replicated
//! partition-update entries built from it or from `spur.conf`.

use std::collections::HashMap;

use spur_core::partition::{Partition, PartitionClears};
use spur_core::partition_limits::{PartitionLimitsUpdate, PartitionResourceLimits};
use spur_core::wal::WalOperation;

use crate::cluster::PartitionError;

/// A partial update to a partition. Unset fields leave the partition unchanged.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PartitionPatch {
    pub nodes: Option<String>,
    pub selector: Option<HashMap<String, String>>,
    pub state: Option<String>,
    pub is_default: Option<bool>,
    /// Raw time string; `INFINITE` / `UNLIMITED` clear the limit.
    pub max_time: Option<String>,
    /// Raw time string; `INFINITE` / `UNLIMITED` clear the limit.
    pub default_time: Option<String>,
    pub max_nodes: Option<u32>,
    /// Takes precedence over `max_nodes`.
    pub clear_max_nodes: bool,
    pub min_nodes: Option<u32>,
    pub allow_accounts: Option<Vec<String>>,
    pub allow_groups: Option<Vec<String>>,
    pub deny_accounts: Option<Vec<String>>,
    pub deny_qos: Option<Vec<String>>,
    pub allow_qos: Option<Vec<String>>,
    pub priority_tier: Option<u32>,
    pub preempt_mode: Option<String>,
    /// `Some(None)` clears the exemption.
    pub preempt_exempt_time: Option<Option<u32>>,
    /// `Some(None)` clears the grace time.
    pub grace_time: Option<Option<u32>>,
    pub limits: PartitionLimitsUpdate,
}

impl PartitionPatch {
    /// Builds the replicated entry for this patch, parsing the raw time strings.
    pub fn into_wal(self, name: &str) -> Result<WalOperation, PartitionError> {
        let (max_time_minutes, clear_max_time) =
            split_clear(parse_clearable_minutes(self.max_time.as_deref(), "time")?);
        let (default_time_minutes, clear_default_time) = split_clear(parse_clearable_minutes(
            self.default_time.as_deref(),
            "default_time",
        )?);
        let (max_nodes, clear_max_nodes) = if self.clear_max_nodes {
            (None, true)
        } else {
            (self.max_nodes, false)
        };
        let (preempt_exempt_time, clear_preempt_exempt_time) =
            split_clear(self.preempt_exempt_time);
        let resource_limits =
            (self.limits != PartitionLimitsUpdate::default()).then_some(self.limits);

        Ok(WalOperation::PartitionUpdate {
            name: name.to_string(),
            nodes: self.nodes,
            selector: self.selector,
            state: self.state,
            max_time_minutes,
            default_time_minutes,
            max_nodes,
            min_nodes: self.min_nodes,
            allow_accounts: self.allow_accounts,
            allow_groups: self.allow_groups,
            deny_accounts: self.deny_accounts,
            deny_qos: self.deny_qos,
            allow_qos: self.allow_qos,
            priority_tier: self.priority_tier,
            preempt_mode: self.preempt_mode,
            is_default: self.is_default,
            preempt_exempt_time,
            grace_time: self.grace_time,
            clear: PartitionClears {
                max_time: clear_max_time,
                default_time: clear_default_time,
                max_nodes: clear_max_nodes,
                preempt_exempt_time: clear_preempt_exempt_time,
            },
            resource_limits,
        })
    }
}

/// The entry that makes a partition match its `spur.conf` definition. A limit the
/// config leaves out is cleared; `preempt_exempt_time` and `grace_time` are only
/// pushed when set, so an unset one keeps whatever an admin applied at runtime.
pub fn wal_from_config(part: &Partition) -> WalOperation {
    let preempt_mode = spur_core::partition::preempt_mode_str(part.preempt_mode);
    WalOperation::PartitionUpdate {
        name: part.name.clone(),
        nodes: Some(part.nodes.clone()),
        selector: Some(part.selector.clone()),
        state: Some(part.state.to_string()),
        max_time_minutes: part.max_time_minutes,
        default_time_minutes: part.default_time_minutes,
        max_nodes: part.max_nodes,
        min_nodes: Some(part.min_nodes),
        allow_accounts: Some(part.allow_accounts.clone()),
        allow_groups: Some(part.allow_groups.clone()),
        deny_accounts: Some(part.deny_accounts.clone()),
        deny_qos: Some(part.deny_qos.clone()),
        allow_qos: Some(part.allow_qos.clone()),
        priority_tier: Some(part.priority_tier),
        preempt_mode: Some(preempt_mode.to_string()),
        is_default: Some(part.is_default),
        preempt_exempt_time: part.preempt_exempt_time,
        grace_time: part.grace_time.map(Some),
        clear: PartitionClears {
            max_time: part.max_time_minutes.is_none(),
            default_time: part.default_time_minutes.is_none(),
            max_nodes: part.max_nodes.is_none(),
            preempt_exempt_time: false,
        },
        resource_limits: Some(PartitionLimitsUpdate::replacing(
            &PartitionResourceLimits::of(part),
        )),
    }
}

/// Splits a clearable value into the value to set and whether to clear.
fn split_clear(value: Option<Option<u32>>) -> (Option<u32>, bool) {
    (value.flatten(), value == Some(None))
}

/// `None` leaves the field alone, `Some(None)` clears it, `Some(Some(m))` sets it.
fn parse_clearable_minutes(
    raw: Option<&str>,
    field: &str,
) -> Result<Option<Option<u32>>, PartitionError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.eq_ignore_ascii_case("INFINITE") || raw.eq_ignore_ascii_case("UNLIMITED") {
        return Ok(Some(None));
    }
    let minutes = spur_core::config::parse_time_minutes(raw)
        .ok_or_else(|| PartitionError::invalid(format!("invalid {field}: {raw}")))?;
    Ok(Some(Some(minutes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Option<Vec<String>> {
        Some(items.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn an_empty_patch_changes_nothing() {
        let wal = PartitionPatch::default().into_wal("gpu").unwrap();
        let WalOperation::PartitionUpdate {
            name,
            nodes,
            selector,
            state,
            max_time_minutes,
            default_time_minutes,
            max_nodes,
            min_nodes,
            allow_accounts,
            allow_groups,
            deny_accounts,
            deny_qos,
            allow_qos,
            priority_tier,
            preempt_mode,
            is_default,
            preempt_exempt_time,
            grace_time,
            clear,
            resource_limits,
        } = wal
        else {
            panic!("expected a PartitionUpdate entry");
        };
        assert_eq!(name, "gpu");
        assert_eq!(nodes, None);
        assert_eq!(selector, None);
        assert_eq!(state, None);
        assert_eq!(max_time_minutes, None);
        assert_eq!(default_time_minutes, None);
        assert_eq!(max_nodes, None);
        assert_eq!(min_nodes, None);
        assert_eq!(allow_accounts, None);
        assert_eq!(allow_groups, None);
        assert_eq!(deny_accounts, None);
        assert_eq!(deny_qos, None);
        assert_eq!(allow_qos, None);
        assert_eq!(priority_tier, None);
        assert_eq!(preempt_mode, None);
        assert_eq!(is_default, None);
        assert_eq!(preempt_exempt_time, None);
        assert_eq!(grace_time, None);
        assert_eq!(clear, PartitionClears::default());
        assert_eq!(resource_limits, None);
    }

    /// The four legacy limits of an entry: the values to set, then the clears.
    #[derive(Debug, PartialEq)]
    struct LimitEntry {
        max_time: Option<u32>,
        default_time: Option<u32>,
        max_nodes: Option<u32>,
        preempt_exempt_time: Option<u32>,
        clear: PartitionClears,
    }

    fn limits_of(wal: WalOperation) -> LimitEntry {
        match wal {
            WalOperation::PartitionUpdate {
                max_time_minutes,
                default_time_minutes,
                max_nodes,
                preempt_exempt_time,
                clear,
                ..
            } => LimitEntry {
                max_time: max_time_minutes,
                default_time: default_time_minutes,
                max_nodes,
                preempt_exempt_time,
                clear,
            },
            other => panic!("expected a PartitionUpdate entry, got {other:?}"),
        }
    }

    fn limit_entry(patch: PartitionPatch) -> LimitEntry {
        limits_of(patch.into_wal("p").unwrap())
    }

    #[test]
    fn infinite_and_unlimited_times_clear_the_limit() {
        for raw in ["INFINITE", "infinite", "UNLIMITED", "Unlimited"] {
            let entry = limit_entry(PartitionPatch {
                max_time: Some(raw.into()),
                default_time: Some(raw.into()),
                ..Default::default()
            });
            assert_eq!(
                entry,
                LimitEntry {
                    max_time: None,
                    default_time: None,
                    max_nodes: None,
                    preempt_exempt_time: None,
                    clear: PartitionClears {
                        max_time: true,
                        default_time: true,
                        ..Default::default()
                    },
                },
                "{raw}"
            );
        }
    }

    #[test]
    fn a_numeric_time_sets_the_limit_in_minutes() {
        let entry = limit_entry(PartitionPatch {
            max_time: Some("90".into()),
            default_time: Some("1-00:00:00".into()),
            ..Default::default()
        });
        assert_eq!(entry.max_time, Some(90));
        assert_eq!(entry.default_time, Some(1440));
        assert_eq!(entry.clear, PartitionClears::default());
    }

    #[test]
    fn an_unset_time_is_left_alone_while_the_other_is_set() {
        let entry = limit_entry(PartitionPatch {
            default_time: Some("30".into()),
            ..Default::default()
        });
        assert_eq!(entry.max_time, None);
        assert_eq!(entry.default_time, Some(30));
        assert_eq!(entry.clear, PartitionClears::default());
    }

    #[test]
    fn an_invalid_max_time_is_rejected_with_the_offending_text() {
        let err = PartitionPatch {
            max_time: Some("soon".into()),
            ..Default::default()
        }
        .into_wal("p")
        .unwrap_err();
        assert_eq!(err, PartitionError::invalid("invalid time: soon"));
    }

    #[test]
    fn an_invalid_default_time_is_rejected_with_the_offending_text() {
        let err = PartitionPatch {
            default_time: Some("later".into()),
            ..Default::default()
        }
        .into_wal("p")
        .unwrap_err();
        assert_eq!(err, PartitionError::invalid("invalid default_time: later"));
    }

    #[test]
    fn max_nodes_sets_clears_or_is_left_alone() {
        let untouched = limit_entry(PartitionPatch::default());
        assert_eq!(untouched.max_nodes, None);
        assert!(!untouched.clear.max_nodes);

        let set = limit_entry(PartitionPatch {
            max_nodes: Some(8),
            ..Default::default()
        });
        assert_eq!(set.max_nodes, Some(8));
        assert!(!set.clear.max_nodes);

        let cleared = limit_entry(PartitionPatch {
            clear_max_nodes: true,
            ..Default::default()
        });
        assert_eq!(cleared.max_nodes, None);
        assert!(cleared.clear.max_nodes);
    }

    #[test]
    fn clearing_max_nodes_wins_over_a_value() {
        let entry = limit_entry(PartitionPatch {
            max_nodes: Some(8),
            clear_max_nodes: true,
            ..Default::default()
        });
        assert_eq!(entry.max_nodes, None);
        assert!(entry.clear.max_nodes);
    }

    #[test]
    fn preempt_exempt_time_sets_clears_or_is_left_alone() {
        let untouched = limit_entry(PartitionPatch::default());
        assert_eq!(untouched.preempt_exempt_time, None);
        assert!(!untouched.clear.preempt_exempt_time);

        let set = limit_entry(PartitionPatch {
            preempt_exempt_time: Some(Some(120)),
            ..Default::default()
        });
        assert_eq!(set.preempt_exempt_time, Some(120));
        assert!(!set.clear.preempt_exempt_time);

        let cleared = limit_entry(PartitionPatch {
            preempt_exempt_time: Some(None),
            ..Default::default()
        });
        assert_eq!(cleared.preempt_exempt_time, None);
        assert!(cleared.clear.preempt_exempt_time);
    }

    #[test]
    fn each_field_lands_in_the_entry_field_of_the_same_name() {
        let selector: HashMap<String, String> = [("rack".to_string(), "r1".to_string())].into();
        let limits = PartitionLimitsUpdate {
            def_mem_per_node: Some(Some(4096)),
            ..Default::default()
        };
        let patch = PartitionPatch {
            nodes: Some("n[1-4]".into()),
            selector: Some(selector.clone()),
            state: Some("DRAIN".into()),
            is_default: Some(true),
            min_nodes: Some(2),
            allow_accounts: strings(&["allow-acct"]),
            allow_groups: strings(&["allow-grp"]),
            deny_accounts: strings(&["deny-acct"]),
            deny_qos: strings(&["deny-qos"]),
            allow_qos: strings(&["allow-qos"]),
            priority_tier: Some(7),
            preempt_mode: Some("REQUEUE".into()),
            preempt_exempt_time: Some(Some(120)),
            grace_time: Some(None),
            limits: limits.clone(),
            ..Default::default()
        };
        let WalOperation::PartitionUpdate {
            nodes,
            selector: wal_selector,
            state,
            min_nodes,
            allow_accounts,
            allow_groups,
            deny_accounts,
            deny_qos,
            allow_qos,
            priority_tier,
            preempt_mode,
            is_default,
            preempt_exempt_time,
            grace_time,
            resource_limits,
            ..
        } = patch.into_wal("p").unwrap()
        else {
            panic!("expected a PartitionUpdate entry");
        };
        assert_eq!(nodes.as_deref(), Some("n[1-4]"));
        assert_eq!(wal_selector, Some(selector));
        assert_eq!(state.as_deref(), Some("DRAIN"));
        assert_eq!(is_default, Some(true));
        assert_eq!(min_nodes, Some(2));
        assert_eq!(allow_accounts, strings(&["allow-acct"]));
        assert_eq!(allow_groups, strings(&["allow-grp"]));
        assert_eq!(deny_accounts, strings(&["deny-acct"]));
        assert_eq!(deny_qos, strings(&["deny-qos"]));
        assert_eq!(allow_qos, strings(&["allow-qos"]));
        assert_eq!(priority_tier, Some(7));
        assert_eq!(preempt_mode.as_deref(), Some("REQUEUE"));
        assert_eq!(preempt_exempt_time, Some(120));
        assert_eq!(grace_time, Some(None));
        assert_eq!(resource_limits, Some(limits));
    }

    #[test]
    fn a_config_partition_without_limits_clears_them() {
        let entry = limits_of(wal_from_config(&Partition::default()));
        assert_eq!(
            entry,
            LimitEntry {
                max_time: None,
                default_time: None,
                max_nodes: None,
                preempt_exempt_time: None,
                clear: PartitionClears {
                    max_time: true,
                    default_time: true,
                    max_nodes: true,
                    preempt_exempt_time: false,
                },
            }
        );
    }

    #[test]
    fn a_config_partition_with_limits_sets_them() {
        let entry = limits_of(wal_from_config(&Partition {
            max_time_minutes: Some(120),
            default_time_minutes: Some(30),
            max_nodes: Some(4),
            preempt_exempt_time: Some(60),
            ..Default::default()
        }));
        assert_eq!(
            entry,
            LimitEntry {
                max_time: Some(120),
                default_time: Some(30),
                max_nodes: Some(4),
                preempt_exempt_time: Some(60),
                clear: PartitionClears::default(),
            }
        );
    }

    #[test]
    fn a_config_partition_leaves_an_unset_grace_time_alone() {
        let grace = |part: &Partition| match wal_from_config(part) {
            WalOperation::PartitionUpdate { grace_time, .. } => grace_time,
            other => panic!("expected a PartitionUpdate entry, got {other:?}"),
        };
        assert_eq!(grace(&Partition::default()), None);
        assert_eq!(
            grace(&Partition {
                grace_time: Some(30),
                ..Default::default()
            }),
            Some(Some(30))
        );
    }
}
