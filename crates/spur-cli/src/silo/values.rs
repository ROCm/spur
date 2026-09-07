// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reading and merging cluster-forge's values documents.
//!
//! cluster-forge keeps every application's Helm values in one `root/values.yaml`, under
//! `.apps.<app>.valuesObject`, with a per-size overlay in `root/values_<size>.yaml`. Assembling the
//! values for a chart means reading that subpath out of each file and deep-merging them.

use anyhow::{Context, Result};
use serde_yaml::Value;

/// Deep-merge `overlay` onto `base`, reproducing the `*` operator of the deployer this replaces.
///
/// Two mappings merge key by key. Everything else takes the overlay: a scalar, an explicit `null`,
/// and a value of a different type all overwrite. A sequence is replaced whole and never appended
/// to — the same rule that silently drops entries when a Helm values list is overridden, so an
/// overlay that gives one list entry removes the rest.
pub fn merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Mapping(mut base), Value::Mapping(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    // Merge in place, so a key the base already holds keeps its position.
                    Some(slot) => {
                        let previous = std::mem::replace(slot, Value::Null);
                        *slot = merge(previous, value);
                    }
                    None => {
                        base.insert(key, value);
                    }
                }
            }
            Value::Mapping(base)
        }
        (_, overlay) => overlay,
    }
}

/// Read a nested key path. `None` when any step of the path is missing.
pub fn at<'a>(doc: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(doc, |node, key| node.get(key))
}

/// An application's Helm values, or `None` when this document does not configure that application.
/// A size overlay names only the applications it changes, so an absent section means "no overlay",
/// not "empty values" — merging `null` would erase the base.
pub fn app_values(doc: &Value, app: &str) -> Option<Value> {
    at(doc, &["apps", app, "valuesObject"]).cloned()
}

/// An application's values from the base document, overlaid with the size-specific document.
pub fn assemble(base: &Value, size_overlay: Option<&Value>, app: &str) -> Value {
    let mut assembled = app_values(base, app).unwrap_or(Value::Null);
    if let Some(overlay) = size_overlay.and_then(|doc| app_values(doc, app)) {
        assembled = merge(assembled, overlay);
    }
    assembled
}

/// Set a nested key, creating any mapping the path passes through.
///
/// cluster-forge ships its placeholders as explicit nulls, so the key is usually there already.
/// Creating what is missing keeps a dropped placeholder from silently losing the value.
pub fn set_at(doc: &mut Value, path: &[&str], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut node = doc;
    for key in parents {
        if !node.is_mapping() {
            *node = Value::Mapping(Default::default());
        }
        let map = node.as_mapping_mut().expect("just made a mapping");
        node = map
            .entry(Value::String((*key).to_string()))
            .or_insert(Value::Mapping(Default::default()));
    }
    if !node.is_mapping() {
        *node = Value::Mapping(Default::default());
    }
    if let Some(map) = node.as_mapping_mut() {
        map.insert(Value::String((*last).to_string()), value);
    }
}

/// cluster-forge writes `.apps.<app>.path` as `<app>/<version>` and keeps the chart at
/// `sources/<app>/<version>`, so the version is the second field.
pub fn chart_version(path_field: &str) -> Option<&str> {
    path_field.split('/').nth(1).filter(|v| !v.is_empty())
}

/// The chart version cluster-forge pins for `app`.
pub fn app_chart_version(doc: &Value, app: &str) -> Result<String> {
    let field = at(doc, &["apps", app, "path"])
        .and_then(Value::as_str)
        .with_context(|| format!("cluster-forge's values name no .apps.{app}.path"))?;
    let version = chart_version(field).with_context(|| {
        format!("cluster-forge's .apps.{app}.path is {field}, which names no chart version")
    })?;
    Ok(version.to_string())
}

pub fn parse(yaml: &str, what: &str) -> Result<Value> {
    serde_yaml::from_str(yaml).with_context(|| format!("could not parse {what}"))
}

pub fn read_file(path: &std::path::Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    parse(&raw, &path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).expect("valid yaml")
    }

    fn merged(base: &str, overlay: &str) -> String {
        serde_yaml::to_string(&merge(yaml(base), yaml(overlay))).expect("serializable")
    }

    // The five rules below are the observed behaviour of `yq eval-all 'select(fileIndex == 0) *
    // select(fileIndex == 1)'`, which is how the deployer this replaces merged a size overlay.

    #[test]
    fn a_scalar_takes_the_overlay_and_a_new_key_is_added() {
        assert_eq!(merged("a: 1\nb: 2", "b: 3\nc: 4"), "a: 1\nb: 3\nc: 4\n");
    }

    #[test]
    fn a_nested_mapping_merges_key_by_key() {
        assert_eq!(
            merged("m:\n  x: 1\n  y: 2", "m:\n  y: 9\n  z: 3"),
            "m:\n  x: 1\n  y: 9\n  z: 3\n"
        );
    }

    #[test]
    fn a_sequence_is_replaced_and_never_appended_to() {
        // The rule behind a Helm list override losing the entries it did not restate.
        assert_eq!(merged("l:\n  - 1\n  - 2\n  - 3", "l:\n  - 9"), "l:\n- 9\n");
    }

    #[test]
    fn an_explicit_null_in_the_overlay_wins() {
        assert_eq!(merged("a: 1", "a: null"), "a: null\n");
    }

    #[test]
    fn a_value_of_another_type_is_replaced_wholesale() {
        assert_eq!(merged("a:\n  x: 1", "a: 5"), "a: 5\n");
        assert_eq!(merged("a: 5", "a:\n  x: 1"), "a:\n  x: 1\n");
    }

    #[test]
    fn an_empty_overlay_changes_nothing() {
        assert_eq!(merged("a: 1", "{}"), "a: 1\n");
    }

    #[test]
    fn a_merged_key_keeps_its_position() {
        // Position is not semantic, but a stable order keeps a rendered ConfigMap free of churn.
        assert_eq!(merged("a: 1\nb: 2\nc: 3", "b: 9"), "a: 1\nb: 9\nc: 3\n");
    }

    #[test]
    fn reads_a_nested_path_and_reports_a_missing_step() {
        let doc = yaml("apps:\n  argocd:\n    path: argocd/8.3.5");
        assert_eq!(
            at(&doc, &["apps", "argocd", "path"]).and_then(Value::as_str),
            Some("argocd/8.3.5")
        );
        assert!(at(&doc, &["apps", "gitea", "path"]).is_none());
        assert!(at(&doc, &["apps", "argocd", "path", "deeper"]).is_none());
    }

    #[test]
    fn takes_the_chart_version_from_the_second_field() {
        assert_eq!(chart_version("argocd/8.3.5"), Some("8.3.5"));
        assert_eq!(chart_version("openbao-config/0.1.0"), Some("0.1.0"));
        assert_eq!(chart_version("argocd"), None);
        assert_eq!(chart_version("argocd/"), None);
    }

    #[test]
    fn an_absent_application_yields_no_overlay_rather_than_null() {
        // Merging a null overlay would erase the base values, so absence must stay distinguishable.
        let doc =
            yaml("apps:\n  argocd:\n    valuesObject:\n      controller:\n        replicas: 1");
        assert!(app_values(&doc, "argocd").is_some());
        assert!(app_values(&doc, "gitea").is_none());
    }

    #[test]
    fn the_bootstrap_domain_loses_to_the_applications_own_global_block() {
        // cluster-forge ships `.apps.argocd.valuesObject.global.domain` as an explicit null, marked
        // "to be filled by cluster-forge app". The overlay therefore clears the bootstrap domain,
        // which is what the deployer this replaces also produced.
        let assembled = merge(
            yaml("global:\n  domain: argocd.example.com"),
            yaml("global:\n  domain: null\nserver:\n  replicas: 1"),
        );
        assert!(at(&assembled, &["global", "domain"])
            .expect("present")
            .is_null());
    }
}
