// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Slurm's QOS level: each account and user stores an edit, and the usable
//! list is that edit applied on top of the parent.
//!
//! An edit is one of three things. Nothing stored means "use the parent".
//! `normal,burst` replaces the parent. `+expedite,-standby` adds and removes
//! without forgetting the parent, so a later change above still flows down.
//! A person is the last step, applied on top of their account.

use std::collections::{HashMap, HashSet};

const ADD_PREFIX: char = '+';
const REMOVE_PREFIX: char = '-';

/// A QOS name starting with a prefix would read as an edit, not a name.
pub(crate) fn is_edit_marker(name: &str) -> bool {
    name.starts_with([ADD_PREFIX, REMOVE_PREFIX])
}

/// What one account or user has stored.
#[derive(Clone, Debug, PartialEq, Eq)]
enum QosLevel {
    Inherit,
    Exact(Vec<String>),
    Adjust {
        add: Vec<String>,
        remove: Vec<String>,
    },
}

/// A `qos` value from `sacctmgr` or the RPC. Absent means the caller did not
/// mention the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QosWrite {
    /// `qos=` with nothing after it: forget this level's edit and inherit.
    Clear,
    /// `qos=a,b`: this level and anyone under it who does not edit further
    /// may use exactly these names.
    Exact(Vec<String>),
    /// `qos+=a` and `qos-=b`, sent as `+a,-b`.
    Delta {
        add: Vec<String>,
        remove: Vec<String>,
    },
}

impl QosWrite {
    pub(crate) fn needs_current(&self) -> bool {
        matches!(self, Self::Delta { .. })
    }

    pub(crate) fn names(&self) -> Vec<&str> {
        match self {
            Self::Clear => Vec::new(),
            Self::Exact(names) => names.iter().map(String::as_str).collect(),
            Self::Delta { add, remove } => add
                .iter()
                .chain(remove.iter())
                .map(String::as_str)
                .collect(),
        }
    }
}

/// One account as stored: the parent link plus the raw edits, not the
/// evaluated result.
#[derive(Clone, Copy)]
pub(crate) struct AccountNode<'a> {
    pub name: &'a str,
    pub parent: Option<&'a str>,
    pub qos: Option<&'a str>,
    pub default_qos: Option<&'a str>,
}

/// The QOS names a job may use, after walking the parents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveQos {
    /// False only when nobody from here up to the top has set a list. An
    /// empty `names` with `limited` set means a list exists and every name
    /// was removed, so no QOS is allowed.
    pub limited: bool,
    pub names: Vec<String>,
}

impl EffectiveQos {
    pub(crate) fn unrestricted() -> Self {
        Self {
            limited: false,
            names: Vec::new(),
        }
    }

    /// `None` when any QOS is allowed. `Some` even when the list is empty.
    pub(crate) fn restriction(&self) -> Option<String> {
        if self.limited {
            Some(self.names.join(","))
        } else {
            None
        }
    }

    pub(crate) fn display(&self) -> String {
        self.names.join(",")
    }
}

/// Evaluated QOS list and default for one account, including what it
/// inherits. The default is the nearest one set on this account or a parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountEffect {
    pub qos: EffectiveQos,
    pub default_qos: Option<String>,
}

/// `None` when the field was left out of the request.
pub(crate) fn parse_write(field: Option<&str>) -> Result<Option<QosWrite>, String> {
    let Some(field) = field else {
        return Ok(None);
    };
    let tokens = split_tokens(field);
    if tokens.is_empty() {
        return Ok(Some(QosWrite::Clear));
    }
    if !tokens.iter().any(|token| is_edit_marker(token)) {
        return Ok(Some(QosWrite::Exact(dedup(tokens))));
    }
    let mut add = Vec::new();
    let mut remove = Vec::new();
    for token in tokens {
        if let Some(name) = token.strip_prefix(ADD_PREFIX) {
            push_named(&mut add, name)?;
            remove.retain(|existing| existing != name);
        } else if let Some(name) = token.strip_prefix(REMOVE_PREFIX) {
            push_named(&mut remove, name)?;
            add.retain(|existing| existing != name);
        } else {
            return Err(format!(
                "qos name '{token}' is mixed with qos+= or qos-=; use one form"
            ));
        }
    }
    Ok(Some(QosWrite::Delta { add, remove }))
}

/// The column value to store. `None` is SQL NULL, which means inherit.
pub(crate) fn apply_write(current: Option<&str>, write: &QosWrite) -> Option<String> {
    let level = match write {
        QosWrite::Clear => QosLevel::Inherit,
        QosWrite::Exact(names) => QosLevel::Exact(names.clone()),
        QosWrite::Delta { add, remove } => apply_delta(&parse_level(current), add, remove),
    };
    encode(&level)
}

pub(crate) fn evaluate_accounts(nodes: &[AccountNode<'_>]) -> HashMap<String, AccountEffect> {
    let mut by_name: HashMap<&str, &AccountNode<'_>> = HashMap::new();
    for node in nodes {
        by_name.insert(node.name, node);
    }
    nodes
        .iter()
        .map(|node| (node.name.to_owned(), effect_of(node.name, &by_name)))
        .collect()
}

/// The user's own edit applied on top of the account's evaluated list.
pub(crate) fn effective_user(user_raw: Option<&str>, account_qos: &EffectiveQos) -> EffectiveQos {
    fold_qos(account_qos, &parse_level(user_raw))
}

fn effect_of(name: &str, by_name: &HashMap<&str, &AccountNode<'_>>) -> AccountEffect {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = Some(name);
    while let Some(current) = cursor {
        if !seen.insert(current) {
            break;
        }
        let Some(node) = by_name.get(current) else {
            break;
        };
        chain.push(*node);
        cursor = node
            .parent
            .filter(|parent| !parent.is_empty() && *parent != current);
    }

    let mut qos = EffectiveQos::unrestricted();
    for node in chain.iter().rev() {
        qos = fold_qos(&qos, &parse_level(node.qos));
    }
    let default_qos = chain.iter().find_map(|node| {
        node.default_qos
            .filter(|qos| !qos.is_empty())
            .map(str::to_owned)
    });
    AccountEffect { qos, default_qos }
}

fn fold_qos(parent: &EffectiveQos, level: &QosLevel) -> EffectiveQos {
    match level {
        QosLevel::Inherit => parent.clone(),
        QosLevel::Exact(names) => EffectiveQos {
            limited: true,
            names: names.clone(),
        },
        QosLevel::Adjust { add, remove } => {
            let mut names: Vec<String> = parent
                .names
                .iter()
                .filter(|name| !remove.iter().any(|gone| gone == *name))
                .cloned()
                .collect();
            for name in add {
                push_unique(&mut names, name);
            }
            EffectiveQos {
                limited: true,
                names,
            }
        }
    }
}

fn apply_delta(current: &QosLevel, add: &[String], remove: &[String]) -> QosLevel {
    match current {
        // A delta on an inherited list must stay a delta. Copying the
        // parent's names would freeze them and stop later parent edits
        // from reaching this level.
        QosLevel::Inherit => {
            let add_names = dedup(add.iter().map(String::as_str));
            let mut remove_names = Vec::new();
            for name in remove {
                if !add_names.iter().any(|kept| kept == name) {
                    push_unique(&mut remove_names, name);
                }
            }
            if add_names.is_empty() && remove_names.is_empty() {
                QosLevel::Inherit
            } else {
                QosLevel::Adjust {
                    add: add_names,
                    remove: remove_names,
                }
            }
        }
        QosLevel::Exact(names) => {
            let mut names = names.clone();
            for name in add {
                push_unique(&mut names, name);
            }
            names.retain(|name| !remove.iter().any(|gone| gone == name));
            // Removing the last name is the same as clearing: inherit again.
            if names.is_empty() {
                QosLevel::Inherit
            } else {
                QosLevel::Exact(names)
            }
        }
        QosLevel::Adjust {
            add: current_add,
            remove: current_remove,
        } => {
            let mut add_names = current_add.clone();
            let mut remove_names = current_remove.clone();
            for name in add {
                push_unique(&mut add_names, name);
                remove_names.retain(|existing| existing != name);
            }
            for name in remove {
                push_unique(&mut remove_names, name);
                add_names.retain(|existing| existing != name);
            }
            if add_names.is_empty() && remove_names.is_empty() {
                QosLevel::Inherit
            } else {
                QosLevel::Adjust {
                    add: add_names,
                    remove: remove_names,
                }
            }
        }
    }
}

fn parse_level(raw: Option<&str>) -> QosLevel {
    match parse_write(raw) {
        Ok(Some(QosWrite::Exact(names))) => QosLevel::Exact(names),
        Ok(Some(QosWrite::Delta { add, remove })) => QosLevel::Adjust { add, remove },
        _ => QosLevel::Inherit,
    }
}

fn encode(level: &QosLevel) -> Option<String> {
    match level {
        QosLevel::Inherit => None,
        QosLevel::Exact(names) if names.is_empty() => None,
        QosLevel::Exact(names) => Some(names.join(",")),
        QosLevel::Adjust { add, remove } if add.is_empty() && remove.is_empty() => None,
        QosLevel::Adjust { add, remove } => {
            let mut parts: Vec<String> = add
                .iter()
                .map(|name| format!("{ADD_PREFIX}{name}"))
                .collect();
            parts.extend(remove.iter().map(|name| format!("{REMOVE_PREFIX}{name}")));
            Some(parts.join(","))
        }
    }
}

fn split_tokens(field: &str) -> Vec<&str> {
    field
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect()
}

fn dedup<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out = Vec::new();
    for name in names {
        push_unique(&mut out, name);
    }
    out
}

fn push_named(names: &mut Vec<String>, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("qos name is empty".to_string());
    }
    push_unique(names, name);
    Ok(())
}

fn push_unique(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|existing| existing == name) {
        names.push(name.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node<'a>(
        name: &'a str,
        parent: Option<&'a str>,
        qos: Option<&'a str>,
        default_qos: Option<&'a str>,
    ) -> AccountNode<'a> {
        AccountNode {
            name,
            parent,
            qos,
            default_qos,
        }
    }

    fn effect<'a>(nodes: &'a [AccountNode<'a>], name: &str) -> AccountEffect {
        evaluate_accounts(nodes)
            .remove(name)
            .expect("account present")
    }

    #[test]
    fn child_inherits_parent_until_it_sets_its_own_list() {
        let nodes = [
            node("root", None, Some("normal"), None),
            node("science", Some("root"), None, None),
            node("physics", Some("science"), Some("burst"), None),
        ];
        let science = effect(&nodes, "science");
        assert_eq!(science.qos.names, vec!["normal".to_string()]);
        let physics = effect(&nodes, "physics");
        assert_eq!(physics.qos.names, vec!["burst".to_string()]);
        assert!(physics.qos.limited);
    }

    #[test]
    fn added_name_keeps_the_inherited_list() {
        let nodes = [
            node("root", None, Some("normal"), None),
            node("science", Some("root"), Some("+expedite"), None),
        ];
        let science = effect(&nodes, "science");
        assert_eq!(
            science.qos.names,
            vec!["normal".to_string(), "expedite".to_string()]
        );
    }

    #[test]
    fn removed_name_drops_out_of_the_inherited_list() {
        let nodes = [
            node("root", None, Some("normal,burst"), None),
            node("science", Some("root"), Some("-burst"), None),
        ];
        assert_eq!(
            effect(&nodes, "science").qos.names,
            vec!["normal".to_string()]
        );
    }

    #[test]
    fn removing_every_inherited_name_allows_nothing() {
        let nodes = [
            node("root", None, Some("normal"), None),
            node("science", Some("root"), Some("-normal"), None),
        ];
        let science = effect(&nodes, "science");
        assert!(science.qos.limited);
        assert!(science.qos.names.is_empty());
        assert_eq!(science.qos.restriction(), Some(String::new()));
    }

    #[test]
    fn unset_tree_is_unrestricted() {
        let nodes = [node("science", Some("root"), None, None)];
        let science = effect(&nodes, "science");
        assert!(!science.qos.limited);
        assert_eq!(science.qos.restriction(), None);
    }

    #[test]
    fn a_cycle_in_parents_stops() {
        let nodes = [
            node("a", Some("b"), Some("normal"), None),
            node("b", Some("a"), None, None),
        ];
        let a = effect(&nodes, "a");
        assert_eq!(a.qos.names, vec!["normal".to_string()]);
    }

    #[test]
    fn nearest_default_wins_and_a_user_edit_is_applied_last() {
        let nodes = [
            node("root", None, Some("normal"), Some("normal")),
            node("physics", Some("root"), None, Some("burst")),
        ];
        let physics = effect(&nodes, "physics");
        assert_eq!(physics.default_qos.as_deref(), Some("burst"));

        let inherited = effective_user(None, &physics.qos);
        assert_eq!(inherited.names, vec!["normal".to_string()]);

        let replaced = effective_user(Some("expedite"), &physics.qos);
        assert_eq!(replaced.names, vec!["expedite".to_string()]);

        let added = effective_user(Some("+expedite"), &physics.qos);
        assert_eq!(
            added.names,
            vec!["normal".to_string(), "expedite".to_string()]
        );
    }

    #[test]
    fn plus_equals_on_an_exact_list_keeps_it_exact() {
        let stored = apply_write(
            Some("burst"),
            &QosWrite::Delta {
                add: vec!["expedite".into()],
                remove: Vec::new(),
            },
        );
        assert_eq!(stored.as_deref(), Some("burst,expedite"));
    }

    #[test]
    fn plus_equals_on_an_inherited_list_stores_only_the_addition() {
        let stored = apply_write(
            None,
            &QosWrite::Delta {
                add: vec!["expedite".into()],
                remove: Vec::new(),
            },
        );
        assert_eq!(stored.as_deref(), Some("+expedite"));
    }

    #[test]
    fn clearing_and_an_empty_value_both_inherit() {
        assert_eq!(apply_write(Some("burst"), &QosWrite::Clear), None);
        assert_eq!(parse_write(Some("")).unwrap(), Some(QosWrite::Clear));
        assert_eq!(parse_write(None).unwrap(), None);
    }

    #[test]
    fn a_list_with_no_names_clears() {
        assert_eq!(parse_write(Some(" , ")).unwrap(), Some(QosWrite::Clear));
    }

    #[test]
    fn adding_then_removing_the_same_name_keeps_the_last_word() {
        let write = parse_write(Some("+burst,-burst")).unwrap().unwrap();
        assert_eq!(
            write,
            QosWrite::Delta {
                add: Vec::new(),
                remove: vec!["burst".to_string()]
            }
        );
    }

    #[test]
    fn removing_the_last_name_of_an_exact_list_inherits_again() {
        let write = QosWrite::Delta {
            add: Vec::new(),
            remove: vec!["burst".to_string()],
        };
        assert_eq!(apply_write(Some("burst"), &write), None);
    }

    #[test]
    fn names_starting_with_a_prefix_are_edit_markers() {
        assert!(is_edit_marker("+burst"));
        assert!(is_edit_marker("-burst"));
        assert!(!is_edit_marker("high-prio"));
    }

    #[test]
    fn a_replacement_cannot_be_mixed_with_an_addition() {
        let err = parse_write(Some("normal,+expedite")).unwrap_err();
        assert!(err.contains("mixed"), "{err}");
    }
}
