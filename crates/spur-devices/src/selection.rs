// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The devices a job was allocated, of every kind.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Stable ids by gres name ("gpu", "nic", ...). A kind with no ids is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceSelection(BTreeMap<String, Vec<u64>>);

impl DeviceSelection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, gres_name: &str, ids: Vec<u64>) -> Self {
        self.set(gres_name, ids);
        self
    }

    pub fn set(&mut self, gres_name: &str, ids: Vec<u64>) {
        if ids.is_empty() {
            self.0.remove(gres_name);
        } else {
            self.0.insert(gres_name.to_string(), ids);
        }
    }

    pub fn of(&self, gres_name: &str) -> &[u64] {
        self.0.get(gres_name).map(Vec::as_slice).unwrap_or_default()
    }

    pub fn gpus(&self) -> &[u64] {
        self.of("gpu")
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u64])> {
        self.0
            .iter()
            .map(|(gres, ids)| (gres.as_str(), ids.as_slice()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_read_back_by_gres_name() {
        let sel = DeviceSelection::new()
            .with("gpu", vec![0x110000, 0x120000])
            .with("nic", vec![0x050000]);
        assert_eq!(sel.gpus(), &[0x110000, 0x120000]);
        assert_eq!(sel.of("nic"), &[0x050000]);
        assert!(sel.of("vnic").is_empty());
    }

    #[test]
    fn an_empty_kind_is_not_kept() {
        let mut sel = DeviceSelection::new().with("gpu", vec![1]);
        sel.set("gpu", Vec::new());
        sel.set("nic", Vec::new());
        assert!(sel.is_empty());
    }

    #[test]
    fn it_serializes_as_a_plain_map() {
        let sel = DeviceSelection::new().with("nic", vec![7]);
        let json = serde_json::to_string(&sel).unwrap();
        assert_eq!(json, r#"{"nic":[7]}"#);
        assert_eq!(serde_json::from_str::<DeviceSelection>(&json).unwrap(), sel);
    }

    #[test]
    fn kinds_iterate_in_name_order() {
        let sel = DeviceSelection::new()
            .with("nic", vec![2])
            .with("gpu", vec![1]);
        let kinds: Vec<&str> = sel.iter().map(|(gres, _)| gres).collect();
        assert_eq!(kinds, vec!["gpu", "nic"]);
    }
}
