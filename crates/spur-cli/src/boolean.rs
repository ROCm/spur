// SPDX-License-Identifier: Apache-2.0

//! Boolean parameter values shared by the CLI parsers.

use anyhow::Result;

/// Parse an explicit on/off parameter. Unrecognised values are rejected rather
/// than treated as off, so a typo cannot silently clear the setting.
pub(crate) fn parse_bool(key: &str, val: &str) -> Result<bool> {
    match val.to_lowercase().as_str() {
        "1" | "yes" | "true" => Ok(true),
        "0" | "no" | "false" => Ok(false),
        _ => Err(anyhow::anyhow!("invalid value for {key}=: '{val}'")),
    }
}

/// Return true when a key=value pair explicitly opts in to a boolean action
/// (value is "1", "yes", or "true", case-insensitive). Bare keys (no `=value`)
/// never match because `parse_params` maps them to an empty string.
pub(crate) fn is_truthy(val: &str) -> bool {
    matches!(val.to_lowercase().as_str(), "1" | "yes" | "true")
}
