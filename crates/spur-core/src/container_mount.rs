// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `--container-mounts` grammar, shared by the controller and both agents.

use std::path::Path;

use thiserror::Error;

/// Where the native agent puts a container's entry script; a user mount here would hide it.
pub const ENTRY_SCRIPT_DIR: &str = "/.spur";

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
#[error("invalid mount '{spec}': {reason}")]
pub struct MountSpecError {
    pub spec: String,
    pub reason: String,
}

/// Splits every value on commas, so a list and a repeated flag mean the same.
/// A blank value is no mounts, so `--container-mounts=$UNSET` keeps working.
pub fn split(values: &[String]) -> Result<Vec<String>, MountSpecError> {
    let mut specs = Vec::new();
    for value in values.iter().filter(|v| !v.trim().is_empty()) {
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

/// Each entry with the spec it was parsed from, so an agent refusing one can quote it.
pub fn parse_list(values: &[String]) -> Result<Vec<(String, MountSpec)>, MountSpecError> {
    let mut mounts: Vec<(String, MountSpec)> = Vec::new();
    for spec in split(values)? {
        let mount = parse(&spec)?;
        let target = mount.target.trim_end_matches('/');
        if mounts
            .iter()
            .any(|(_, m)| m.target.trim_end_matches('/') == target)
        {
            return Err(invalid(
                &spec,
                "another entry already mounts this destination",
            ));
        }
        mounts.push((spec, mount));
    }
    Ok(mounts)
}

/// The split list, each entry checked, in the form the agents receive.
pub fn normalize(values: &[String]) -> Result<Vec<String>, MountSpecError> {
    Ok(parse_list(values)?
        .into_iter()
        .map(|(spec, _)| spec)
        .collect())
}

pub fn parse(spec: &str) -> Result<MountSpec, MountSpecError> {
    let parts: Vec<&str> = spec.split(':').map(str::trim).collect();
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
    check_target(spec, target)?;
    let mut mount = MountSpec {
        source,
        target: target.to_string(),
        readonly: false,
    };
    apply_flags(spec, &mut mount, flags)?;
    Ok(mount)
}

fn check_target(spec: &str, target: &str) -> Result<(), MountSpecError> {
    if !target.starts_with('/') {
        return Err(invalid(spec, "destination must be an absolute path"));
    }
    if target.contains('=') {
        return Err(invalid(spec, "destination cannot contain '='"));
    }
    let target = target.trim_end_matches('/');
    if target.is_empty() {
        return Err(invalid(spec, "cannot mount over the container root"));
    }
    if Path::new(target).starts_with(ENTRY_SCRIPT_DIR) {
        return Err(invalid(
            spec,
            &format!("{ENTRY_SCRIPT_DIR} is reserved for the job's entry script"),
        ));
    }
    Ok(())
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

    fn mounts(values: &[&str]) -> Result<Vec<MountSpec>, MountSpecError> {
        let values: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        Ok(parse_list(&values)?.into_iter().map(|(_, m)| m).collect())
    }

    #[test]
    fn a_comma_list_is_one_mount_per_entry() {
        assert_eq!(
            mounts(&["/data:/data,/scratch:/scratch,/models:/models:ro"]).unwrap(),
            vec![
                path("/data", "/data", false),
                path("/scratch", "/scratch", false),
                path("/models", "/models", true),
            ]
        );
    }

    #[test]
    fn two_listed_mounts_are_not_one_mount_onto_a_comma_named_target() {
        assert_eq!(
            mounts(&["/a:/b,/c:/d"]).unwrap(),
            vec![path("/a", "/b", false), path("/c", "/d", false)]
        );
    }

    #[test]
    fn a_blank_value_is_no_mounts() {
        assert_eq!(normalize(&["".into()]).unwrap(), Vec::<String>::new());
        assert_eq!(
            normalize(&[" ".into(), "/a:/a".into()]).unwrap(),
            vec!["/a:/a"]
        );
    }

    #[test]
    fn spaces_around_fields_are_ignored() {
        assert_eq!(
            mounts(&["/a : /b : ro"]).unwrap(),
            vec![path("/a", "/b", true)]
        );
    }

    #[test]
    fn a_list_and_a_repeated_flag_combine_in_order() {
        let specs = normalize(&["/a:/a, /b:/b".into(), "/c:/c".into()]).unwrap();
        assert_eq!(specs, vec!["/a:/a", "/b:/b", "/c:/c"]);
    }

    #[test]
    fn every_rejection_names_the_offending_spec() {
        for (value, named) in [
            ("/a:/a,", "/a:/a,"),
            ("/a:/a,,/b:/b", "/a:/a,,/b:/b"),
            ("/a:/a,nocolon", "nocolon"),
            ("/src:/dst:z", "/src:/dst:z"),
            ("/src:/dst:ro:rw", "/src:/dst:ro:rw"),
            ("relative:/dst", "relative:/dst"),
            ("/src::ro", "/src::ro"),
            ("pvc:claim", "pvc:claim"),
        ] {
            let err = normalize(&[value.to_string()]).unwrap_err().to_string();
            assert!(err.contains(&format!("'{named}'")), "{value} gave: {err}");
        }
    }

    #[test]
    fn a_destination_must_be_an_absolute_path_below_the_root_without_an_option() {
        for spec in [
            "tmpfs:size=1G",
            "/src:ro",
            "pvc:claim:ro",
            "/src:relative",
            "/src:/",
            "tmpfs://",
            "tmpfs:/size=1G",
            "/src:/dst=ro",
        ] {
            assert!(parse(spec).is_err(), "{spec} should be rejected");
        }
    }

    #[test]
    fn the_entry_script_folder_cannot_be_mounted_over() {
        for spec in ["tmpfs:/.spur", "/src:/.spur/", "/src:/.spur/x"] {
            let err = parse(spec).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{spec} gave: {err}");
        }
        assert!(parse("/src:/.spurx").is_ok());
    }

    #[test]
    fn two_entries_cannot_share_a_destination() {
        let err = normalize(&["/a:/data,tmpfs:/data/".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("'tmpfs:/data/'"), "{err}");
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
