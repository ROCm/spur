// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The named input of `ClusterManager::update_partition`.

use std::collections::HashMap;

use spur_core::partition_limits::PartitionLimitsUpdate;
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
        let max_time_minutes = parse_clearable_minutes(self.max_time.as_deref(), "time")?;
        let default_time_minutes =
            parse_clearable_minutes(self.default_time.as_deref(), "default_time")?;
        let max_nodes = if self.clear_max_nodes {
            Some(None)
        } else {
            self.max_nodes.map(Some)
        };
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
            preempt_exempt_time: self.preempt_exempt_time,
            grace_time: self.grace_time,
            resource_limits,
        })
    }
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
    use crate::cluster::PartitionError;
    use spur_core::partition_limits::PartitionLimitsUpdate;
    use spur_core::wal::WalOperation;

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
        assert_eq!(resource_limits, None);
    }

    fn max_and_default_time(patch: PartitionPatch) -> (Option<Option<u32>>, Option<Option<u32>>) {
        match patch.into_wal("p").unwrap() {
            WalOperation::PartitionUpdate {
                max_time_minutes,
                default_time_minutes,
                ..
            } => (max_time_minutes, default_time_minutes),
            other => panic!("expected a PartitionUpdate entry, got {other:?}"),
        }
    }

    #[test]
    fn infinite_and_unlimited_times_clear_the_limit() {
        for raw in ["INFINITE", "infinite", "UNLIMITED", "Unlimited"] {
            let (max, default) = max_and_default_time(PartitionPatch {
                max_time: Some(raw.into()),
                default_time: Some(raw.into()),
                ..Default::default()
            });
            assert_eq!(max, Some(None), "max_time {raw}");
            assert_eq!(default, Some(None), "default_time {raw}");
        }
    }

    #[test]
    fn a_numeric_time_sets_the_limit_in_minutes() {
        let (max, default) = max_and_default_time(PartitionPatch {
            max_time: Some("90".into()),
            default_time: Some("1-00:00:00".into()),
            ..Default::default()
        });
        assert_eq!(max, Some(Some(90)));
        assert_eq!(default, Some(Some(1440)));
    }

    #[test]
    fn an_unset_time_is_left_alone_while_the_other_is_set() {
        let (max, default) = max_and_default_time(PartitionPatch {
            default_time: Some("30".into()),
            ..Default::default()
        });
        assert_eq!(max, None);
        assert_eq!(default, Some(Some(30)));
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

    fn max_nodes_of(patch: PartitionPatch) -> Option<Option<u32>> {
        match patch.into_wal("p").unwrap() {
            WalOperation::PartitionUpdate { max_nodes, .. } => max_nodes,
            other => panic!("expected a PartitionUpdate entry, got {other:?}"),
        }
    }

    #[test]
    fn max_nodes_sets_clears_or_is_left_alone() {
        assert_eq!(max_nodes_of(PartitionPatch::default()), None);
        assert_eq!(
            max_nodes_of(PartitionPatch {
                max_nodes: Some(8),
                ..Default::default()
            }),
            Some(Some(8))
        );
        assert_eq!(
            max_nodes_of(PartitionPatch {
                clear_max_nodes: true,
                ..Default::default()
            }),
            Some(None)
        );
    }

    #[test]
    fn clearing_max_nodes_wins_over_a_value() {
        let max_nodes = max_nodes_of(PartitionPatch {
            max_nodes: Some(8),
            clear_max_nodes: true,
            ..Default::default()
        });
        assert_eq!(max_nodes, Some(None));
    }

    #[test]
    fn each_field_lands_in_the_entry_field_of_the_same_name() {
        let selector: std::collections::HashMap<String, String> =
            [("rack".to_string(), "r1".to_string())].into();
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
        assert_eq!(preempt_exempt_time, Some(Some(120)));
        assert_eq!(grace_time, Some(None));
        assert_eq!(resource_limits, Some(limits));
    }
}
