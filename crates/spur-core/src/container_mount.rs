// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `--container-mounts` grammar, shared by the controller and both agents.
//!
//! One value holds one or more comma-separated mounts:
//!
//! - `<src>:<dst>[:ro|:rw]` bind-mounts an absolute host path
//! - `tmpfs:<dst>[:size=<n>[K|M|G]][:ro|:rw]` mounts an empty in-memory filesystem
//! - `pvc:<claim>:<dst>[:ro|:rw]` mounts a PersistentVolumeClaim (Kubernetes agent only)

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSource {
    Path(String),
    Tmpfs { size_bytes: Option<u64> },
    Pvc(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    pub source: MountSource,
    pub target: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid mount spec '{spec}': {reason}")]
pub struct MountSpecError {
    pub spec: String,
    pub reason: String,
}

/// Splits every value on commas, so a list and a repeated flag mean the same.
pub fn split(values: &[String]) -> Result<Vec<String>, MountSpecError> {
    let mut specs = Vec::new();
    for value in values {
        for entry in value.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                return Err(invalid(value, "empty entry in the mount list"));
            }
            specs.push(entry.to_string());
        }
    }
    Ok(specs)
}

pub fn parse_list(values: &[String]) -> Result<Vec<MountSpec>, MountSpecError> {
    split(values)?.iter().map(|spec| parse(spec)).collect()
}

/// The split list, each entry checked, in the form the agents receive.
pub fn normalize(values: &[String]) -> Result<Vec<String>, MountSpecError> {
    let specs = split(values)?;
    for spec in &specs {
        parse(spec)?;
    }
    Ok(specs)
}

pub fn parse(spec: &str) -> Result<MountSpec, MountSpecError> {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(invalid(spec, "empty field"));
    }
    let (source, target, flags) = match parts.as_slice() {
        ["tmpfs", target, flags @ ..] => (MountSource::Tmpfs { size_bytes: None }, *target, flags),
        ["pvc", claim, target, flags @ ..] => (MountSource::Pvc(claim.to_string()), *target, flags),
        ["pvc", ..] => return Err(invalid(spec, "expected pvc:<claim>:<dst>")),
        [src, target, flags @ ..] if src.starts_with('/') => {
            (MountSource::Path(src.to_string()), *target, flags)
        }
        [_, _, ..] => return Err(invalid(spec, "source must be an absolute path")),
        _ => return Err(invalid(spec, "expected <src>:<dst>[:ro]")),
    };
    let mut mount = MountSpec {
        source,
        target: target.to_string(),
        readonly: false,
    };
    apply_flags(spec, &mut mount, flags)?;
    Ok(mount)
}

fn apply_flags(spec: &str, mount: &mut MountSpec, flags: &[&str]) -> Result<(), MountSpecError> {
    let mut access = None;
    for &flag in flags {
        match flag {
            "ro" | "rw" => {
                if access.is_some_and(|seen| seen != flag) {
                    return Err(invalid(spec, "ro and rw together"));
                }
                access = Some(flag);
            }
            _ if flag.starts_with("size=") => {
                let MountSource::Tmpfs { size_bytes } = &mut mount.source else {
                    return Err(invalid(spec, "size= only applies to a tmpfs mount"));
                };
                if size_bytes.is_some() {
                    return Err(invalid(spec, "size= given twice"));
                }
                *size_bytes = Some(parse_size(spec, &flag["size=".len()..])?);
            }
            _ => return Err(invalid(spec, &format!("unknown flag '{flag}'"))),
        }
    }
    mount.readonly = access == Some("ro");
    Ok(())
}

fn parse_size(spec: &str, raw: &str) -> Result<u64, MountSpecError> {
    let (digits, unit) = match raw.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => raw.split_at(at),
        None => (raw, ""),
    };
    let shift = match unit {
        "" => 0,
        "K" | "k" => 10,
        "M" | "m" => 20,
        "G" | "g" => 30,
        _ => {
            return Err(invalid(
                spec,
                "size takes a number with an optional K, M or G",
            ))
        }
    };
    digits
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .and_then(|n| n.checked_mul(1u64 << shift))
        .ok_or_else(|| invalid(spec, "size must be a positive number of bytes"))
}

fn invalid(spec: &str, reason: &str) -> MountSpecError {
    MountSpecError {
        spec: spec.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(src: &str, dst: &str, readonly: bool) -> MountSpec {
        MountSpec {
            source: MountSource::Path(src.into()),
            target: dst.into(),
            readonly,
        }
    }

    #[test]
    fn a_comma_list_is_one_mount_per_entry() {
        let mounts = parse_list(&["/data:/data,/scratch:/scratch,/models:/models:ro".into()]);
        assert_eq!(
            mounts.unwrap(),
            vec![
                path("/data", "/data", false),
                path("/scratch", "/scratch", false),
                path("/models", "/models", true),
            ]
        );
    }

    #[test]
    fn two_listed_mounts_are_not_one_mount_onto_a_comma_named_target() {
        let mounts = parse_list(&["/a:/b,/c:/d".into()]).unwrap();
        assert_eq!(
            mounts,
            vec![path("/a", "/b", false), path("/c", "/d", false)]
        );
    }

    #[test]
    fn a_list_and_a_repeated_flag_combine_in_order() {
        let specs = normalize(&["/a:/a, /b:/b".into(), "/c:/c".into()]).unwrap();
        assert_eq!(specs, vec!["/a:/a", "/b:/b", "/c:/c"]);
    }

    #[test]
    fn every_rejection_names_the_offending_spec() {
        for (values, named) in [
            (vec!["/a:/a,".to_string()], "/a:/a,"),
            (vec!["/a:/a,nocolon".to_string()], "nocolon"),
            (vec!["/src:/dst:z".to_string()], "/src:/dst:z"),
            (vec!["/src:/dst:ro:rw".to_string()], "/src:/dst:ro:rw"),
            (vec!["relative:/dst".to_string()], "relative:/dst"),
            (vec!["/src::ro".to_string()], "/src::ro"),
            (vec!["pvc:claim".to_string()], "pvc:claim"),
            (vec!["".to_string()], "''"),
        ] {
            let err = normalize(&values).unwrap_err().to_string();
            assert!(err.contains(named), "{values:?} gave: {err}");
        }
    }

    #[test]
    fn ro_and_rw_are_exact_flags() {
        assert!(parse("/src:/dst:ro").unwrap().readonly);
        assert!(!parse("/src:/dst:rw").unwrap().readonly);
        assert!(!parse("/src:/dst").unwrap().readonly);
        assert!(parse("/src:/dst:error").is_err());
    }

    #[test]
    fn a_tmpfs_takes_an_optional_size() {
        assert_eq!(
            parse("tmpfs:/cache").unwrap().source,
            MountSource::Tmpfs { size_bytes: None }
        );
        assert_eq!(
            parse("tmpfs:/cache:size=64M").unwrap().source,
            MountSource::Tmpfs {
                size_bytes: Some(64 << 20)
            }
        );
        assert_eq!(
            parse("tmpfs:/cache:size=2G:ro").unwrap(),
            MountSpec {
                source: MountSource::Tmpfs {
                    size_bytes: Some(2 << 30)
                },
                target: "/cache".into(),
                readonly: true,
            }
        );
        assert_eq!(
            parse("tmpfs:/cache:size=4096").unwrap().source,
            MountSource::Tmpfs {
                size_bytes: Some(4096)
            }
        );
    }

    #[test]
    fn a_bad_size_is_rejected() {
        for spec in [
            "tmpfs:/c:size=0",
            "tmpfs:/c:size=64MB",
            "tmpfs:/c:size=",
            "tmpfs:/c:size=1M:size=2M",
            "tmpfs:/c:size=99999999999999999999G",
            "/src:/dst:size=64M",
        ] {
            assert!(parse(spec).is_err(), "{spec} should be rejected");
        }
    }

    #[test]
    fn a_pvc_keeps_its_claim_and_access() {
        assert_eq!(
            parse("pvc:datasets:/data:ro").unwrap(),
            MountSpec {
                source: MountSource::Pvc("datasets".into()),
                target: "/data".into(),
                readonly: true,
            }
        );
    }
}
