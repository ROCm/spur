// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! kubectl-style plugins: `spur <name> ...` runs `spur-<name>` from `PATH`
//! when `<name>` is not a built-in command.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A plugin found on `PATH`.
pub struct Plugin {
    /// Command name as the user types it, e.g. `silo` or `silo-debug`.
    pub name: String,
    pub path: PathBuf,
}

fn path_dirs() -> Vec<PathBuf> {
    match std::env::var_os("PATH") {
        Some(p) => std::env::split_paths(&p).collect(),
        None => Vec::new(),
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn lookup(dirs: &[PathBuf], file: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(file)).find(|p| is_executable(p))
}

/// Longest match among `spur-a-b-c`, `spur-a-b`, `spur-a`. An underscore in the
/// file name stands for a dash in the command name, as kubectl does.
/// Returns the plugin and the arguments left over for it.
pub fn resolve(dirs: &[PathBuf], args: &[String]) -> Option<(Plugin, Vec<String>)> {
    for n in (1..=args.len()).rev() {
        let name = args[..n].join("-");
        let underscored = args[..n]
            .iter()
            .map(|a| a.replace('-', "_"))
            .collect::<Vec<_>>()
            .join("-");
        let Some(path) = lookup(dirs, &format!("spur-{name}"))
            .or_else(|| lookup(dirs, &format!("spur-{underscored}")))
        else {
            continue;
        };
        return Some((Plugin { name, path }, args[n..].to_vec()));
    }
    None
}

/// Every `spur-*` executable on `PATH`, first occurrence of a name wins.
pub fn list(dirs: &[PathBuf]) -> Vec<Plugin> {
    let mut found: Vec<Plugin> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
                continue;
            };
            let Some(rest) = file.strip_prefix("spur-") else {
                continue;
            };
            if rest.is_empty() || !is_executable(&path) {
                continue;
            }
            let name = rest.replace('_', "-");
            if found.iter().any(|p| p.name == name) {
                continue;
            }
            found.push(Plugin { name, path });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Replace this process with the plugin. Exit codes and signals pass unchanged.
#[cfg(unix)]
pub fn exec(plugin: &Plugin, args: &[String]) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let conf =
        std::env::var_os("SPUR_CONF").unwrap_or_else(|| OsString::from("/etc/spur/spur.conf"));
    let bin = std::env::current_exe()
        .map(OsString::from)
        .unwrap_or_else(|_| OsString::from("spur"));

    std::process::Command::new(&plugin.path)
        .args(args)
        .env("SPUR_CONF", conf)
        .env("SPUR_BIN", bin)
        .env("SPUR_VERSION", spur_core::version::version_string())
        .env("SPUR_PLUGIN_NAME", &plugin.name)
        .exec()
}

/// `spur <name> ...` with no built-in match: run a plugin or explain why not.
pub fn dispatch(args: &[String], is_builtin: impl Fn(&str) -> bool) -> ! {
    let dirs = path_dirs();
    if let Some((plugin, rest)) = resolve(&dirs, args) {
        let err = exec(&plugin, &rest);
        eprintln!("spur: cannot run {}: {err}", plugin.path.display());
        std::process::exit(126);
    }
    eprintln!("spur: unknown command '{}'", args[0]);
    eprintln!("spur: no plugin named spur-{} on PATH either", args[0]);
    let shadowed: Vec<String> = list(&dirs)
        .into_iter()
        .filter(|p| is_builtin(&p.name))
        .map(|p| p.name)
        .collect();
    if !shadowed.is_empty() {
        eprintln!(
            "spur: these plugins shadow a built-in command and never run: {}",
            shadowed.join(", ")
        );
    }
    std::process::exit(1);
}

/// `spur plugin list`.
pub fn cmd_list(is_builtin: impl Fn(&str) -> bool) {
    let plugins = list(&path_dirs());
    if plugins.is_empty() {
        println!("no spur-* plugins found on PATH");
        return;
    }
    for p in &plugins {
        if is_builtin(&p.name) {
            println!(
                "{:<20} {} (shadowed: {} is a built-in command)",
                p.name,
                p.path.display(),
                p.name
            );
        } else {
            println!("{:<20} {}", p.name, p.path.display());
        }
    }
}

/// Names for the `help` output.
pub fn names() -> Vec<String> {
    list(&path_dirs()).into_iter().map(|p| p.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn touch_exec(dir: &Path, name: &str) {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn longest_match_wins() {
        let dir = tempfile::tempdir().unwrap();
        touch_exec(dir.path(), "spur-silo");
        touch_exec(dir.path(), "spur-silo-debug");
        let dirs = vec![dir.path().to_path_buf()];

        let (plugin, rest) = resolve(&dirs, &args(&["silo", "debug", "x"])).unwrap();
        assert_eq!(plugin.name, "silo-debug");
        assert_eq!(rest, args(&["x"]));
    }

    #[test]
    fn shorter_match_when_no_longer_one_exists() {
        let dir = tempfile::tempdir().unwrap();
        touch_exec(dir.path(), "spur-silo");
        let dirs = vec![dir.path().to_path_buf()];

        let (plugin, rest) = resolve(&dirs, &args(&["silo", "install", "aiwb-demo"])).unwrap();
        assert_eq!(plugin.name, "silo");
        assert_eq!(rest, args(&["install", "aiwb-demo"]));
    }

    #[test]
    fn underscore_in_file_name_matches_a_dash_in_the_command() {
        let dir = tempfile::tempdir().unwrap();
        touch_exec(dir.path(), "spur-my_tool");
        let dirs = vec![dir.path().to_path_buf()];

        let (plugin, rest) = resolve(&dirs, &args(&["my-tool", "run"])).unwrap();
        assert_eq!(plugin.name, "my-tool");
        assert_eq!(rest, args(&["run"]));
    }

    #[test]
    fn no_match_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        touch_exec(dir.path(), "spur-silo");
        let dirs = vec![dir.path().to_path_buf()];

        assert!(resolve(&dirs, &args(&["nosuch"])).is_none());
    }

    #[test]
    fn a_file_without_the_executable_bit_is_not_a_plugin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("spur-silo"), "#!/bin/sh\n").unwrap();
        let dirs = vec![dir.path().to_path_buf()];

        assert!(resolve(&dirs, &args(&["silo"])).is_none());
        assert!(list(&dirs).is_empty());
    }

    #[test]
    fn the_first_path_entry_wins() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        touch_exec(first.path(), "spur-silo");
        touch_exec(second.path(), "spur-silo");
        let dirs = vec![first.path().to_path_buf(), second.path().to_path_buf()];

        let (plugin, _) = resolve(&dirs, &args(&["silo"])).unwrap();
        assert_eq!(plugin.path, first.path().join("spur-silo"));
        assert_eq!(list(&dirs).len(), 1);
    }

    #[test]
    fn list_reports_every_plugin_by_command_name() {
        let dir = tempfile::tempdir().unwrap();
        touch_exec(dir.path(), "spur-silo");
        touch_exec(dir.path(), "spur-my_tool");
        touch_exec(dir.path(), "spurious");
        touch_exec(dir.path(), "spur-");
        let dirs = vec![dir.path().to_path_buf()];

        let names: Vec<String> = list(&dirs).into_iter().map(|p| p.name).collect();
        assert_eq!(names, args(&["my-tool", "silo"]));
    }
}
