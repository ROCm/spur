// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::job::JobSpec;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Constraints<'a> {
    pub features: Vec<&'a str>,
    pub same_fabric: bool,
}

impl<'a> Constraints<'a> {
    pub fn parse(value: Option<&'a str>) -> Result<Self, String> {
        let value = value.unwrap_or_default();
        let typed = value.contains("same(")
            || (value.contains("gpu.") && value.contains(['(', ')', '=', '<', '>', '!']));
        if !typed {
            return Ok(Self {
                features: value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect(),
                same_fabric: false,
            });
        }
        let mut result = Self::default();
        for term in value.split([',', '&']).map(str::trim) {
            match term {
                "same(gpu.vpod_id)" | "same(gpu.fabric_domain_id)" => result.same_fabric = true,
                feature if !feature.is_empty() && !feature.contains(['(', ')', '|', '=', '<', '>', '!']) => result.features.push(feature),
                _ => return Err(format!("unsupported GPU constraint {term:?}; use same(gpu.fabric_domain_id) or same(gpu.vpod_id), optionally joined with node features using ',' or '&'")),
            }
        }
        Ok(result)
    }
}

pub fn requires_same_fabric(spec: &JobSpec) -> bool {
    Constraints::parse(spec.constraint.as_deref()).is_ok_and(|c| c.same_fabric)
}

pub fn validate(spec: &JobSpec) -> Result<(), String> {
    let constraints = Constraints::parse(spec.constraint.as_deref())?;
    if constraints.same_fabric
        && crate::gpu_request::resolve_gpu_demand(spec)
            .map_err(|e| e.to_string())?
            .is_none()
        && !spec.exclusive
    {
        return Err(
            "same GPU fabric constraint requires a GPU request or whole-node exclusivity".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fabric_predicates_are_separate_from_node_features() {
        for name in ["vpod_id", "fabric_domain_id"] {
            let value = format!("fast&same(gpu.{name}),large");
            let parsed = Constraints::parse(Some(&value)).unwrap();
            assert_eq!(parsed.features, ["fast", "large"]);
            assert!(parsed.same_fabric);
        }
        assert_eq!(
            Constraints::parse(Some("a,, b")).unwrap().features,
            ["a", "b"]
        );
        assert_eq!(Constraints::parse(Some("a&b")).unwrap().features, ["a&b"]);
        assert_eq!(
            Constraints::parse(Some("gpu.fast")).unwrap().features,
            ["gpu.fast"]
        );
    }

    #[test]
    fn unsupported_expressions_never_drop_a_requirement() {
        for value in [
            "same(gpu.vpod_id)|fast",
            "same(gpu.vpod_id)&",
            "gpu.vram>4",
            "same(gpu.unknown)",
            "same (gpu.vpod_id)",
            "same(gpu.vpod_id)=false",
        ] {
            assert!(Constraints::parse(Some(value)).is_err(), "{value}");
        }
    }
}
