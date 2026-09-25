// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn write_exec(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn spur(path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spur"))
        .args(args)
        .env("PATH", path)
        .env_remove("SPUR_CONF")
        .output()
        .expect("failed to spawn spur")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn plugin_gets_remaining_args_env_and_keeps_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    write_exec(
        dir.path(),
        "spur-aims",
        "#!/bin/sh\necho \"args=$*\"\necho \"name=$SPUR_PLUGIN_NAME\"\necho \"conf=$SPUR_CONF\"\nexit 3\n",
    );

    let out = spur(dir.path(), &["aims", "install", "demo"]);

    assert_eq!(out.status.code(), Some(3));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("args=install demo"), "{stdout}");
    assert!(stdout.contains("name=aims"), "{stdout}");
    assert!(stdout.contains("conf=/etc/spur/spur.conf"), "{stdout}");
}

#[test]
fn plugin_that_cannot_start_exits_126() {
    let dir = tempfile::tempdir().unwrap();
    write_exec(dir.path(), "spur-broken", "#!/nonexistent/interpreter\n");

    let out = spur(dir.path(), &["broken"]);

    assert_eq!(out.status.code(), Some(126));
    assert!(text(&out.stderr).contains("cannot run"));
}

#[test]
fn unknown_command_without_plugin_fails() {
    let dir = tempfile::tempdir().unwrap();

    let out = spur(dir.path(), &["nosuch"]);

    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("unknown command 'nosuch'"), "{stderr}");
    assert!(stderr.contains("no plugin named spur-nosuch"), "{stderr}");
}

#[test]
fn plugin_list_marks_shadowed_builtins() {
    let dir = tempfile::tempdir().unwrap();
    write_exec(dir.path(), "spur-aims", "#!/bin/sh\n");
    write_exec(dir.path(), "spur-queue", "#!/bin/sh\n");

    for args in [&["plugin"][..], &["plugin", "list"]] {
        let out = spur(dir.path(), args);

        assert!(out.status.success());
        let stdout = text(&out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 2, "{stdout}");
        assert!(lines[0].starts_with("aims ") && !lines[0].contains("shadowed"));
        assert!(lines[1].starts_with("queue ") && lines[1].contains("shadowed"));
    }
}

#[test]
fn plugin_list_without_plugins() {
    let dir = tempfile::tempdir().unwrap();

    let out = spur(dir.path(), &["plugin", "list"]);

    assert!(out.status.success());
    assert_eq!(text(&out.stdout), "no spur-* plugins found on PATH\n");
}

#[test]
fn unknown_plugin_subcommand_fails() {
    let dir = tempfile::tempdir().unwrap();

    let out = spur(dir.path(), &["plugin", "install"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("unknown plugin command 'install'"));
}

#[test]
fn help_lists_plugins_only_when_present() {
    let dir = tempfile::tempdir().unwrap();

    let out = spur(dir.path(), &["help"]);
    assert!(!text(&out.stdout).contains("Plugins found on PATH"));

    write_exec(dir.path(), "spur-aims", "#!/bin/sh\n");
    write_exec(dir.path(), "spur-my_tool", "#!/bin/sh\n");
    let out = spur(dir.path(), &["help"]);
    assert!(text(&out.stdout).contains("Plugins found on PATH: aims my-tool"));
}
