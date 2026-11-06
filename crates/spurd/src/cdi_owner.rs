// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Keeps a Spur-written `/etc/cdi/amd.json` in sync with live GPU discovery;
//! a spec without Spur's `auto-detected` annotation is someone else's.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use tracing::{info, warn};

use spur_devices::cdi::annotations::{AUTO_DETECTED, STABLE_ID};
use spur_devices::cdi::discovery::discover_to_cdi;
use spur_devices::cdi::spec::CdiSpec;

pub const SPEC_PATH: &str = "/etc/cdi/amd.json";

const SPEC_FILENAME: &str = "amd.json";

const AMD_CDI_KIND: &str = "amd.com/gpu";

static LAST_WARNED_FOREIGN: Mutex<Option<Vec<String>>> = Mutex::new(None);

pub fn is_spur_owned(spec: &CdiSpec) -> bool {
    spec.annotations.get(AUTO_DETECTED).map(String::as_str) == Some("true")
}

fn gpu_stable_ids(specs: &[CdiSpec]) -> HashSet<&str> {
    specs
        .iter()
        .filter(|spec| spec.kind == AMD_CDI_KIND)
        .flat_map(|spec| &spec.devices)
        .filter_map(|device| device.annotations.get(STABLE_ID).map(String::as_str))
        .collect()
}

/// A failed live scan (`live` empty) must never make a good spec look stale.
pub fn is_stale(on_disk: &CdiSpec, live: &[CdiSpec]) -> bool {
    if live.is_empty() {
        return false;
    }
    gpu_stable_ids(std::slice::from_ref(on_disk)) != gpu_stable_ids(live)
}

pub fn write_specs(dir: &Path, specs: &[CdiSpec]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating CDI spec dir {}", dir.display()))?;
    for (i, spec) in specs.iter().enumerate() {
        spec.validate()
            .map_err(|e| anyhow::anyhow!("invalid CDI spec: {e}"))?;
        let final_path = if i == 0 {
            dir.join(SPEC_FILENAME)
        } else {
            dir.join(format!("amd-{i}.json"))
        };
        let tmp = PathBuf::from(format!("{}.tmp", final_path.display()));
        spec.write_json(&tmp)
            .map_err(|e| anyhow::anyhow!("write CDI spec: {e}"))?;
        std::fs::rename(&tmp, &final_path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), final_path.display()))?;
    }
    Ok(())
}

fn warn_foreign_stale(on_disk: &CdiSpec) {
    let mut ids: Vec<String> = gpu_stable_ids(std::slice::from_ref(on_disk))
        .into_iter()
        .map(str::to_owned)
        .collect();
    ids.sort();

    let mut last = LAST_WARNED_FOREIGN
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if last.as_ref() == Some(&ids) {
        return;
    }
    warn!(
        path = SPEC_PATH,
        "CDI spec is stale against live GPU discovery but is not Spur-owned; not rewriting"
    );
    *last = Some(ids);
}

/// Inner seam so tests can supply `live` without touching host GPUs. `resync` calls this with
/// `discover_to_cdi()`.
pub fn resync_with(dir: &Path, auto_detect: bool, live: &[CdiSpec]) -> bool {
    if !auto_detect {
        return false;
    }
    let on_disk = match CdiSpec::read_file(&dir.join(SPEC_FILENAME)) {
        Ok(spec) => spec,
        Err(_) => return false,
    };
    if !is_stale(&on_disk, live) {
        return false;
    }
    if !is_spur_owned(&on_disk) {
        warn_foreign_stale(&on_disk);
        return false;
    }
    match write_specs(dir, live) {
        Ok(()) => {
            info!(count = live.len(), "rewrote stale Spur-owned CDI spec");
            true
        }
        Err(e) => {
            warn!(error = %e, "failed to rewrite stale Spur-owned CDI spec");
            false
        }
    }
}

pub fn resync(dir: &Path, auto_detect: bool) -> bool {
    resync_with(dir, auto_detect, &discover_to_cdi())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use spur_devices::cdi::spec::{CdiDevice, GENERATED_CDI_VERSION};

    use super::*;

    fn gpu_device(stable_id: &str) -> CdiDevice {
        let mut annotations = HashMap::new();
        annotations.insert(STABLE_ID.to_string(), stable_id.to_string());
        CdiDevice {
            name: stable_id.to_string(),
            annotations,
            container_edits: None,
        }
    }

    fn spec(auto_detected: bool, stable_ids: &[&str]) -> CdiSpec {
        let mut annotations = HashMap::new();
        if auto_detected {
            annotations.insert(AUTO_DETECTED.to_string(), "true".to_string());
        }
        CdiSpec {
            cdi_version: GENERATED_CDI_VERSION.into(),
            kind: AMD_CDI_KIND.into(),
            annotations,
            devices: stable_ids.iter().map(|id| gpu_device(id)).collect(),
            container_edits: None,
        }
    }

    #[test]
    fn a_spur_owned_spec_with_other_ids_is_stale() {
        let on_disk = spec(true, &["1"]);
        let live = vec![spec(true, &["2"])];
        assert!(is_stale(&on_disk, &live));
    }

    #[test]
    fn a_spec_with_the_live_ids_is_not_stale() {
        let on_disk = spec(true, &["1", "2"]);
        let live = vec![spec(true, &["2", "1"])];
        assert!(!is_stale(&on_disk, &live));
    }

    #[test]
    fn a_failed_scan_is_never_stale() {
        let on_disk = spec(true, &["1"]);
        assert!(!is_stale(&on_disk, &[]));
    }

    #[test]
    fn a_foreign_spec_is_never_spur_owned() {
        let foreign = spec(false, &["1"]);
        assert!(!is_spur_owned(&foreign));
        let owned = spec(true, &["1"]);
        assert!(is_spur_owned(&owned));
    }

    #[test]
    fn write_specs_replaces_the_spec_atomically_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let specs = vec![spec(true, &["1", "2"])];

        write_specs(dir.path(), &specs).expect("write_specs");

        let path = dir.path().join(SPEC_FILENAME);
        assert!(path.exists());
        assert!(!dir.path().join(format!("{SPEC_FILENAME}.tmp")).exists());
        let round_tripped = CdiSpec::read_file(&path).expect("read back written spec");
        assert_eq!(round_tripped, specs[0]);
    }

    #[test]
    fn resync_rewrites_a_spur_owned_stale_spec() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_specs(dir.path(), &[spec(true, &["1"])]).expect("seed spec");

        let live = vec![spec(true, &["2"])];
        let rewrote = resync_with(dir.path(), true, &live);

        assert!(rewrote);
        let on_disk = CdiSpec::read_file(&dir.path().join(SPEC_FILENAME)).expect("read spec");
        assert_eq!(on_disk, live[0]);
    }

    #[test]
    fn resync_skips_a_foreign_spec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let foreign = spec(false, &["1"]);
        write_specs(dir.path(), std::slice::from_ref(&foreign)).expect("seed foreign spec");

        let live = vec![spec(true, &["2"])];
        let rewrote = resync_with(dir.path(), true, &live);

        assert!(!rewrote);
        let on_disk = CdiSpec::read_file(&dir.path().join(SPEC_FILENAME)).expect("read spec");
        assert_eq!(on_disk, foreign);
    }

    #[test]
    fn resync_is_a_noop_when_auto_detect_is_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_specs(dir.path(), &[spec(true, &["1"])]).expect("seed spec");

        let live = vec![spec(true, &["2"])];
        assert!(!resync_with(dir.path(), false, &live));
    }

    #[test]
    fn resync_is_a_noop_when_the_spec_is_not_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_specs(dir.path(), &[spec(true, &["1"])]).expect("seed spec");

        let live = vec![spec(true, &["1"])];
        assert!(!resync_with(dir.path(), true, &live));
    }
}
