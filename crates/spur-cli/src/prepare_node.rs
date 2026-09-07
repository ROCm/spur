// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur k8s prepare-node`: make a bare node ready for a k0s cluster.
//!
//! Runs locally as root and needs no controller, so it works before any cluster exists. Every step
//! is idempotent: a second run reports what is already in place and changes nothing.

use anyhow::{bail, Context, Result};

use spur_core::k0s::K0S_DATA_DIR;

/// Marks what this command owns, so an operator can tell it from a hand-written entry.
const MANAGED_TAG: &str = "# managed by spur";

const FSTAB_PATH: &str = "/etc/fstab";

/// cluster-forge grows the k0s data directory well past a stock root disk, so a smaller device is
/// worth a warning. Not an error: a cluster that never installs the platform stack needs far less.
const MIN_RECOMMENDED_GB: u64 = 500;

/// What one install of the platform stack needs free, rather than what a node should have.
///
/// Measured at 141G for size medium on a single-node cluster. A node that starts below this runs
/// out part way, and a full disk reads as an image pull that hangs rather than as a disk problem.
const MIN_FREE_GB: u64 = 150;

pub async fn cmd_prepare_node(
    data_disk: Option<String>,
    force_format: bool,
    dry_run: bool,
) -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        bail!(
            "spur k8s prepare-node changes disks, firewall rules and kernel limits, so it must run \
             as root"
        );
    }
    match data_disk.as_deref() {
        Some(device) => prepare_data_disk(device, force_format, dry_run).await?,
        None => {
            eprintln!(
                "No --data-disk given, so the k0s data directory stays on the root filesystem."
            )
        }
    }
    open_firewall(dry_run).await?;
    raise_kernel_limits(dry_run)?;
    ensure_time_sync(dry_run).await
}

/// Give k0s its own filesystem at [`K0S_DATA_DIR`].
///
/// This must happen before the node ever runs k0s. k0s records absolute paths for every kubelet
/// volume, so a data directory that moves onto another device later breaks each of those mounts.
async fn prepare_data_disk(device: &str, force_format: bool, dry_run: bool) -> Result<()> {
    validate_device_path(device)?;
    if !std::path::Path::new(device).exists() {
        bail!("{device} does not exist");
    }

    if is_mount_point(K0S_DATA_DIR)? {
        let current = mount_source_of(&read_mounts()?, K0S_DATA_DIR);
        match current.as_deref() {
            Some(src) if src == device => eprintln!("{K0S_DATA_DIR} already holds {device}"),
            Some(src) => eprintln!("{K0S_DATA_DIR} already holds {src}, not {device}"),
            None => eprintln!("{K0S_DATA_DIR} is already a mount point"),
        }
        warn_if_small(dry_run).await;
        return Ok(());
    }

    if let Some(target) = mount_target_of(&read_mounts()?, device) {
        bail!("{device} is already mounted at {target} — unmount it first");
    }
    if directory_has_entries(K0S_DATA_DIR)? {
        bail!(
            "{K0S_DATA_DIR} already holds data on the root filesystem. Moving it onto {device} now \
             breaks every kubelet volume mount, so prepare this node before it runs k0s."
        );
    }

    let existing = filesystem_type(device).await?;
    match existing.as_deref() {
        Some("ext4") => eprintln!("{device} already carries an ext4 filesystem"),
        Some(other) if !force_format => bail!(
            "{device} carries a {other} filesystem. Pass --force-format to erase it, or choose \
             another device."
        ),
        _ => format_ext4(device, dry_run).await?,
    }

    let uuid = if dry_run {
        String::from("<uuid-after-format>")
    } else {
        device_uuid(device).await?
    };
    ensure_fstab_entry(&uuid, dry_run)?;

    if dry_run {
        eprintln!("Would create {K0S_DATA_DIR} and mount {device} there");
        return Ok(());
    }
    std::fs::create_dir_all(K0S_DATA_DIR)
        .with_context(|| format!("could not create {K0S_DATA_DIR}"))?;
    run_checked("mount", &[K0S_DATA_DIR]).await?;
    if !is_mount_point(K0S_DATA_DIR)? {
        bail!("mount reported success but {K0S_DATA_DIR} is not a mount point");
    }
    eprintln!("Mounted {device} at {K0S_DATA_DIR}");
    warn_if_small(false).await;
    Ok(())
}

async fn format_ext4(device: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        eprintln!("Would erase {device} and build an ext4 filesystem on it");
        return Ok(());
    }
    eprintln!("Building an ext4 filesystem on {device} ...");
    run_checked("wipefs", &["-a", device]).await?;
    run_checked("mkfs.ext4", &["-F", "-q", device]).await?;
    Ok(())
}

/// Add the boot-time mount, keyed by UUID so a device rename cannot point it at another disk.
/// `nofail` keeps a missing disk from holding up the boot.
fn ensure_fstab_entry(uuid: &str, dry_run: bool) -> Result<()> {
    let fstab = std::fs::read_to_string(FSTAB_PATH)
        .with_context(|| format!("could not read {FSTAB_PATH}"))?;
    match fstab_uuid_for(&fstab, K0S_DATA_DIR) {
        Some(existing) if existing == uuid => {
            eprintln!("{FSTAB_PATH} already mounts {K0S_DATA_DIR} from this device");
            return Ok(());
        }
        Some(existing) => bail!(
            "{FSTAB_PATH} already mounts {K0S_DATA_DIR} from UUID={existing}. Remove that line if \
             it is stale, then run this again."
        ),
        None => {}
    }
    let line = fstab_entry(uuid);
    if dry_run {
        eprintln!("Would append to {FSTAB_PATH}: {line}");
        return Ok(());
    }
    let mut updated = fstab;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&line);
    updated.push('\n');
    std::fs::write(FSTAB_PATH, updated).with_context(|| format!("could not write {FSTAB_PATH}"))?;
    eprintln!("Added {K0S_DATA_DIR} to {FSTAB_PATH}");
    Ok(())
}

async fn warn_if_small(dry_run: bool) {
    if dry_run {
        return;
    }
    let Some(gb) = free_gb(K0S_DATA_DIR).await else {
        return;
    };
    if gb < MIN_RECOMMENDED_GB {
        eprintln!(
            "warning: {K0S_DATA_DIR} has {gb}G free; the platform stack wants about \
             {MIN_RECOMMENDED_GB}G"
        );
    }
}

// -- firewall ----------------------------------------------------------------------------------

/// Ports that must reach this node. k0s's own set, not RKE2's: SPUR runs no supervisor (9345),
/// no docker socket (2376) and no canal health port (9099), and etcd's client port is loopback.
const TCP_PORTS: &[&str] = &[
    "80",          // platform-stack gateway
    "443",         // platform-stack gateway
    "2380",        // etcd peer (HA control plane)
    "6443",        // kube-apiserver — also the path a pod takes to the cluster's own service IP
    "6817",        // spurctld
    "6818",        // spurd
    "6821",        // spurctld raft
    "8132",        // konnectivity
    "9443",        // k0s join API (HA control plane)
    "10250",       // kubelet
    "30000:32767", // NodePort
];

const UDP_PORTS: &[&str] = &["30000:32767"];

/// `-m multiport` accepts at most 15 ports, and a range counts as two.
const MULTIPORT_LIMIT: usize = 15;

/// The unit that puts SPUR's own ACCEPT rules back after a reboot.
const FIREWALL_UNIT: &str = "spur-k0s-firewall.service";
const FIREWALL_UNIT_PATH: &str = "/etc/systemd/system/spur-k0s-firewall.service";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Backend {
    Legacy,
    NfTables,
    Unknown,
}

/// A sanity check on the node the install is about to run on.
///
/// The install assumes the node is configured, and this changes nothing: it names what looks wrong
/// and carries on. That is worth the few seconds because each of these fails late and as something
/// else — an exhausted inotify pool as a kubelet that silently stops tracking ConfigMap updates, a
/// full disk as an image pull that never finishes.
///
/// Which device holds the data directory is not checked. A node that keeps it on the root
/// filesystem is a supported choice, so only the free space on it matters.
pub async fn unmet_prerequisites() -> Vec<String> {
    let mut missing = Vec::new();
    if let Some(gb) = free_gb(K0S_DATA_DIR).await {
        if gb < MIN_FREE_GB {
            missing.push(format!(
                "{K0S_DATA_DIR} has {gb}G free, below the {MIN_FREE_GB}G the platform stack needs"
            ));
        }
    }
    for (proto, ports) in [("tcp", TCP_PORTS), ("udp", UDP_PORTS)] {
        if !accept_rule_present(proto, ports).await {
            missing.push(format!(
                "the {proto} ports k0s needs are not accepted on INPUT ({})",
                multiport_spec(ports)
            ));
        }
    }
    for &(key, floor) in SYSCTL_FLOORS {
        match read_sysctl(key) {
            Ok(Some(live)) if live < floor => missing.push(format!(
                "{key} is {live}, below the {floor} the platform stack needs"
            )),
            // A kernel without the key, or one that cannot be read, is not something the operator
            // can fix by preparing the node, so it is not reported as unfinished work.
            _ => {}
        }
    }
    if !chrony_socket_present() {
        missing.push(format!(
            "nothing serves {CHRONY_SOCKET}, which the platform stack's chrony exporter reads"
        ));
    }
    missing
}

/// Open the ports k0s and SPUR need, and leave the node's own catch-all rule in place. SPUR adds
/// what the cluster cannot work without and takes nothing away, so a node keeps whatever policy it
/// arrived with. The FORWARD chain is left untouched: the CNI inserts its own rules there.
async fn open_firewall(dry_run: bool) -> Result<()> {
    let backend = firewall_preflight().await?;
    match backend {
        Backend::NfTables => eprintln!("iptables is in use, on the nftables backend"),
        Backend::Legacy => eprintln!("iptables is in use, on the legacy backend"),
        Backend::Unknown => eprintln!("iptables is in use, on an unrecognised backend"),
    }
    report_blocking_rules().await?;
    ensure_accept_rule("tcp", TCP_PORTS, dry_run).await?;
    ensure_accept_rule("udp", UDP_PORTS, dry_run).await?;
    persist_rules(dry_run).await
}

/// Prove that `iptables` works on this node before SPUR edits anything. SPUR speaks iptables and
/// nothing else, so a node without it is an error the operator resolves, not a case SPUR guesses at.
async fn firewall_preflight() -> Result<Backend> {
    let version = run("iptables", &["--version"]).await.map_err(|_| {
        anyhow::anyhow!(
            "iptables is not available, so SPUR cannot open the ports k0s needs. Install \
             iptables, then run this again."
        )
    })?;
    if !version.status.success() {
        bail!("iptables is present but `iptables --version` failed");
    }
    let backend = parse_backend(&String::from_utf8_lossy(&version.stdout));

    // A read of the live chain proves the backend answers, not just that the binary runs.
    let listed = run("iptables", &["-S", "INPUT"]).await?;
    if !listed.status.success() {
        bail!(
            "could not read the INPUT chain: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }

    Ok(backend)
}

async fn report_blocking_rules() -> Result<()> {
    for chain in ["INPUT", "FORWARD"] {
        let out = run("iptables", &["-S", chain]).await?;
        let rules = String::from_utf8_lossy(&out.stdout);
        if has_catch_all_reject(&rules) {
            eprintln!("{chain} ends in a catch-all REJECT/DROP");
        }
    }
    Ok(())
}

/// The one ACCEPT rule SPUR adds for a protocol. `verb` picks what to do with it: `-C` tests, `-I`
/// inserts.
fn accept_rule_args(verb: &str, proto: &str, spec: &str) -> Vec<String> {
    [
        verb,
        "INPUT",
        "-p",
        proto,
        "-m",
        "multiport",
        "--dports",
        spec,
        "-j",
        "ACCEPT",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Whether the rule is already in INPUT. A node with no usable `iptables` reads as not prepared,
/// which is the answer that sends the operator to `prepare-node`, where the real error is raised.
async fn accept_rule_present(proto: &str, ports: &[&str]) -> bool {
    let args = accept_rule_args("-C", proto, &multiport_spec(ports));
    matches!(run_args("iptables", &args).await, Ok(out) if out.status.success())
}

/// Insert at the head of INPUT so the rule lands ahead of any catch-all, whatever else the chain
/// holds. `-C` first, so a second run adds nothing.
async fn ensure_accept_rule(proto: &str, ports: &[&str], dry_run: bool) -> Result<()> {
    let spec = multiport_spec(ports);
    if multiport_entries(ports) > MULTIPORT_LIMIT {
        bail!("too many {proto} ports for one multiport rule: {spec}");
    }
    let args = |verb: &'static str| accept_rule_args(verb, proto, &spec);
    if run_args("iptables", &args("-C")).await?.status.success() {
        eprintln!("{proto} ports already accepted: {spec}");
        return Ok(());
    }
    if dry_run {
        eprintln!("Would accept {proto} ports on INPUT: {spec}");
        return Ok(());
    }
    let mut insert = args("-I");
    insert.insert(2, "1".to_string());
    run_args_checked("iptables", &insert).await?;
    eprintln!("Accepted {proto} ports on INPUT: {spec}");
    Ok(())
}

/// Re-apply SPUR's own rules after a reboot, and nothing else.
///
/// iptables rules live in kernel memory, so a reboot empties the chain and something has to put
/// them back. The obvious route, `iptables-save` into the distribution's `rules.v4`, writes the
/// **whole** live ruleset, which makes SPUR responsible for rules it does not own. On a node where
/// the cluster already runs, that snapshot picks up the CNI's chains, and those reference ipsets
/// the CNI creates at start-up. `iptables-restore` resolves every line before it applies any, so
/// one unresolvable set leaves the boot with no rules at all — SPUR's included.
///
/// A unit of SPUR's own carries only the two ACCEPT rules and tests for each before inserting it,
/// so it stays additive and cannot be broken by, or break, another tool's ruleset.
async fn persist_rules(dry_run: bool) -> Result<()> {
    if dry_run {
        eprintln!("Would install {FIREWALL_UNIT} to re-apply these rules at boot");
        return Ok(());
    }
    std::fs::write(FIREWALL_UNIT_PATH, firewall_unit())
        .with_context(|| format!("could not write {FIREWALL_UNIT_PATH}"))?;
    run_checked("systemctl", &["daemon-reload"]).await?;
    run_checked("systemctl", &["enable", FIREWALL_UNIT]).await?;
    eprintln!("Enabled {FIREWALL_UNIT} to re-apply these rules at boot");
    Ok(())
}

/// The unit text. `After=netfilter-persistent.service` matters: that unit flushes the chain before
/// it restores, so a rule inserted ahead of it would not survive.
fn firewall_unit() -> String {
    let mut unit = String::from(
        "[Unit]\n\
         Description=SPUR: accept the ports a k0s cluster needs\n\
         After=netfilter-persistent.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n",
    );
    for (proto, ports) in [("tcp", TCP_PORTS), ("udp", UDP_PORTS)] {
        unit.push_str(&format!("ExecStart={}\n", reapply_command(proto, ports)));
    }
    unit.push_str("\n[Install]\nWantedBy=multi-user.target\n");
    unit
}

/// Test for the rule, insert it only when it is absent. This is the same `-C` then `-I` the command
/// itself runs, so a boot changes nothing on a node that already carries the rule.
fn reapply_command(proto: &str, ports: &[&str]) -> String {
    let spec = multiport_spec(ports);
    let rule = format!("INPUT -p {proto} -m multiport --dports {spec} -j ACCEPT");
    format!("/bin/sh -c 'iptables -C {rule} 2>/dev/null || iptables -I {rule}'")
}

// -- kernel limits -----------------------------------------------------------------------------

/// A drop-in of SPUR's own keeps these apart from the distribution's files, so a second run is a
/// plain overwrite instead of an append that grows every time.
const SYSCTL_DROP_IN: &str = "/etc/sysctl.d/90-spur-k0s.conf";

/// Floors, not targets: a node that already sits higher keeps its value.
///
/// An inotify limit counts per UID on the host, and a container gets no namespace of its own, so
/// kubelet, containerd and every pod that runs as root share one pool. The platform stack is full
/// of config watchers and exhausts Ubuntu's stock 128 instances. `inotify_init1` then fails with
/// `EMFILE`, which reads as "too many open files" and looks like a file-descriptor limit instead.
/// The failure arrives late, once enough pods run, and a kubelet that cannot open a watcher stops
/// tracking ConfigMap and Secret updates without any pod crashing.
const SYSCTL_FLOORS: &[(&str, u64)] = &[
    ("fs.inotify.max_user_instances", 8192),
    ("fs.inotify.max_user_watches", 524288),
];

fn raise_kernel_limits(dry_run: bool) -> Result<()> {
    let mut persisted = Vec::new();
    for &(key, floor) in SYSCTL_FLOORS {
        let Some(live) = read_sysctl(key)? else {
            eprintln!("warning: this kernel has no {key}, so SPUR left it alone");
            continue;
        };
        if live >= floor {
            eprintln!("{key} is already {live}");
        } else if dry_run {
            eprintln!("Would raise {key} from {live} to {floor}");
        } else {
            write_sysctl(key, floor)?;
            eprintln!("Raised {key} from {live} to {floor}");
        }
        persisted.push((key, live.max(floor)));
    }
    persist_sysctls(&persisted, dry_run)
}

fn persist_sysctls(entries: &[(&str, u64)], dry_run: bool) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    if dry_run {
        eprintln!("Would save the kernel limits to {SYSCTL_DROP_IN}");
        return Ok(());
    }
    if let Some(dir) = std::path::Path::new(SYSCTL_DROP_IN).parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    std::fs::write(SYSCTL_DROP_IN, sysctl_drop_in(entries))
        .with_context(|| format!("could not write {SYSCTL_DROP_IN}"))?;
    eprintln!("Saved the kernel limits to {SYSCTL_DROP_IN}");
    Ok(())
}

/// Read a sysctl through `/proc/sys` rather than the `sysctl` binary, which a minimal image can
/// omit. `None` means this kernel does not carry the key.
fn read_sysctl(key: &str) -> Result<Option<u64>> {
    let path = sysctl_path(key);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("could not read {path}")),
    };
    let value = raw
        .trim()
        .parse()
        .with_context(|| format!("{path} does not hold a number: {}", raw.trim()))?;
    Ok(Some(value))
}

fn write_sysctl(key: &str, value: u64) -> Result<()> {
    let path = sysctl_path(key);
    std::fs::write(&path, format!("{value}\n")).with_context(|| format!("could not write {path}"))
}

// -- time sync ---------------------------------------------------------------------------------

const CHRONY_PACKAGE: &str = "chrony";

/// The socket the platform stack's chrony exporter reads. It is a hostPath mount in the
/// otel-lgtm-stack chart, and the exporter is given this exact path, so it is the requirement
/// rather than "the node keeps time somehow". systemd-timesyncd keeps a correct clock and creates
/// no socket, which leaves the exporter unhealthy and its application degraded.
const CHRONY_SOCKET: &str = "/run/chrony/chronyd.sock";

/// Give the node chrony, because the platform stack reads chrony's socket.
///
/// A right clock matters on its own — skew invalidates a TLS certificate, an OIDC token and an
/// etcd lease — but any NTP client would do for that. The socket is what makes it chrony.
async fn ensure_time_sync(dry_run: bool) -> Result<()> {
    if chrony_socket_present() {
        eprintln!("chrony serves {CHRONY_SOCKET}");
        return Ok(());
    }
    install_chrony(dry_run).await
}

pub(crate) fn chrony_socket_present() -> bool {
    std::path::Path::new(CHRONY_SOCKET).exists()
}

/// Install chrony. apt is the only package manager SPUR speaks, which matches the Debian and Ubuntu
/// paths this command already writes.
///
/// The package disables systemd-timesyncd on Ubuntu, so the node ends with one time source rather
/// than two competing ones.
async fn install_chrony(dry_run: bool) -> Result<()> {
    if dry_run {
        eprintln!("Would install {CHRONY_PACKAGE}, because nothing serves {CHRONY_SOCKET}");
        return Ok(());
    }
    eprintln!("Nothing serves {CHRONY_SOCKET}, so SPUR is installing {CHRONY_PACKAGE} ...");
    apt(&["update"]).await?;
    apt(&["install", "-y", CHRONY_PACKAGE]).await?;
    run_checked("systemctl", &["enable", "--now", CHRONY_PACKAGE]).await?;
    if !chrony_socket_present() {
        eprintln!("warning: {CHRONY_PACKAGE} is installed but {CHRONY_SOCKET} is not there yet");
    }
    eprintln!("Installed {CHRONY_PACKAGE} and started it. The clock converges over a few minutes.");
    Ok(())
}

/// `apt-get` with the prompts turned off, so an install cannot stop on a question nobody answers.
async fn apt(args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("apt-get")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .args(args)
        .output()
        .await
        .context("could not run apt-get — install chrony by hand")?;
    if !out.status.success() {
        bail!(
            "apt-get {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub(crate) fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

// -- node queries ------------------------------------------------------------------------------

fn read_mounts() -> Result<String> {
    std::fs::read_to_string("/proc/mounts").context("could not read /proc/mounts")
}

fn is_mount_point(path: &str) -> Result<bool> {
    Ok(mount_source_of(&read_mounts()?, path).is_some())
}

/// `blkid` exits non-zero when the device carries no filesystem, which is not an error here.
async fn filesystem_type(device: &str) -> Result<Option<String>> {
    let out = run("blkid", &["-s", "TYPE", "-o", "value", device]).await?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((out.status.success() && !value.is_empty()).then_some(value))
}

async fn device_uuid(device: &str) -> Result<String> {
    let out = run_checked("blkid", &["-s", "UUID", "-o", "value", device]).await?;
    let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uuid.is_empty() {
        bail!("{device} reports no UUID after formatting");
    }
    Ok(uuid)
}

fn directory_has_entries(path: &str) -> Result<bool> {
    let dir = std::path::Path::new(path);
    if !dir.is_dir() {
        return Ok(false);
    }
    let mut entries = std::fs::read_dir(dir).with_context(|| format!("could not read {path}"))?;
    Ok(entries.next().is_some())
}

async fn run(program: &str, args: &[&str]) -> Result<std::process::Output> {
    tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("could not run {program}"))
}

async fn run_args(program: &str, args: &[String]) -> Result<std::process::Output> {
    tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("could not run {program}"))
}

async fn run_args_checked(program: &str, args: &[String]) -> Result<std::process::Output> {
    let out = run_args(program, args).await?;
    if !out.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

async fn run_checked(program: &str, args: &[&str]) -> Result<std::process::Output> {
    let out = run(program, args).await?;
    if !out.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

// -- pure helpers ------------------------------------------------------------------------------

fn sysctl_path(key: &str) -> String {
    format!("/proc/sys/{}", key.replace('.', "/"))
}

/// Write the value SPUR settled on, not the floor. Pinning the floor would lower a node whose
/// kernel scaled the default higher, the next time the boot-time apply runs.
fn sysctl_drop_in(entries: &[(&str, u64)]) -> String {
    let mut body = format!("{MANAGED_TAG}\n");
    for (key, value) in entries {
        body.push_str(&format!("{key} = {value}\n"));
    }
    body
}

fn validate_device_path(device: &str) -> Result<()> {
    if !device.starts_with("/dev/") {
        bail!("--data-disk must be a device path under /dev/, got {device}");
    }
    if device.contains(char::is_whitespace) {
        bail!("--data-disk must not contain whitespace");
    }
    Ok(())
}

fn fstab_entry(uuid: &str) -> String {
    format!("UUID={uuid} {K0S_DATA_DIR} ext4 defaults,nofail 0 2 {MANAGED_TAG}")
}

/// The UUID an existing `/etc/fstab` line mounts at `mount_point`, if any. Returns the literal
/// source for a non-UUID line so the caller can report the conflict rather than duplicate it.
fn fstab_uuid_for(fstab: &str, mount_point: &str) -> Option<String> {
    for line in fstab.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        // A blank line yields no fields. Skip it: `?` here would end the whole search, so an
        // entry below the first blank line would go unseen and be duplicated.
        let Some(source) = fields.next() else {
            continue;
        };
        if fields.next() != Some(mount_point) {
            continue;
        }
        return Some(source.strip_prefix("UUID=").unwrap_or(source).to_string());
    }
    None
}

/// The device mounted at `path`, read from `/proc/mounts` (field 1 source, field 2 target).
fn mount_source_of(mounts: &str, path: &str) -> Option<String> {
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let source = fields.next()?;
        (fields.next()? == path).then(|| source.to_string())
    })
}

fn mount_target_of(mounts: &str, device: &str) -> Option<String> {
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == device).then(|| fields.next().map(str::to_string))?
    })
}

/// `iptables --version` prints e.g. `iptables v1.8.10 (nf_tables)`. The backend does not change
/// what SPUR does — the nft-backed front-end programs the same chains — but naming it in the
/// output tells an operator which ruleset was touched.
fn parse_backend(version_stdout: &str) -> Backend {
    if version_stdout.contains("nf_tables") {
        return Backend::NfTables;
    }
    if version_stdout.contains("legacy") {
        return Backend::Legacy;
    }
    Backend::Unknown
}

/// A catch-all is a REJECT or DROP with no match conditions, i.e. `-A INPUT -j REJECT ...`.
/// A rule that matches a port or an interface is somebody's deliberate policy, not a blanket block.
fn has_catch_all_reject(iptables_s_stdout: &str) -> bool {
    iptables_s_stdout.lines().any(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("-A") {
            return false;
        }
        let _chain = fields.next();
        matches!(fields.next(), Some("-j"))
            && matches!(fields.next(), Some("REJECT") | Some("DROP"))
    })
}

fn multiport_spec(ports: &[&str]) -> String {
    ports.join(",")
}

/// A range costs two of the fifteen slots `-m multiport` allows.
fn multiport_entries(ports: &[&str]) -> usize {
    ports
        .iter()
        .map(|p| if p.contains(':') { 2 } else { 1 })
        .sum()
}

/// Free space on the filesystem that holds `path`, in whole GB.
async fn free_gb(path: &str) -> Option<u64> {
    let probe = nearest_existing(std::path::Path::new(path))?;
    let out = run("df", &["-BG", "--output=avail", &probe.to_string_lossy()])
        .await
        .ok()?;
    out.status
        .success()
        .then(|| avail_gb(&String::from_utf8_lossy(&out.stdout)))?
}

/// The nearest ancestor of `path` that exists, `path` itself included.
///
/// The data directory may not be there yet: the install reads its free space before k0s has ever
/// run, and k0s is what creates it. An ancestor sits on the filesystem the directory will land on,
/// so its free space is the right answer.
fn nearest_existing(path: &std::path::Path) -> Option<&std::path::Path> {
    let mut probe = path;
    while !probe.exists() {
        probe = probe.parent()?;
    }
    Some(probe)
}

/// Read `df -BG --output=avail`, which prints a header and then a value like `1907G`.
fn avail_gb(df_stdout: &str) -> Option<u64> {
    df_stdout
        .lines()
        .nth(1)?
        .trim()
        .trim_end_matches('G')
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_unit_carries_only_spurs_own_rules() {
        // The whole point of the unit: a snapshot of the live table makes SPUR responsible for the
        // CNI's chains, and one unresolvable ipset there costs every rule at the next boot.
        let unit = firewall_unit();
        assert_eq!(unit.matches("ExecStart=").count(), 2);
        assert!(!unit.contains("iptables-restore"));
        assert!(!unit.contains("kube"));
        for proto in ["tcp", "udp"] {
            assert!(unit.contains(&format!("-p {proto} -m multiport")));
        }
    }

    #[test]
    fn the_boot_unit_tests_before_it_inserts() {
        // A unit that inserted unconditionally would add a duplicate rule on every boot.
        let command = reapply_command("tcp", TCP_PORTS);
        let (test, insert) = command.split_once("||").expect("a test and an insert");
        assert!(test.contains("iptables -C INPUT"));
        assert!(insert.contains("iptables -I INPUT"));
        assert!(test.contains(&multiport_spec(TCP_PORTS)));
    }

    #[test]
    fn the_boot_unit_runs_after_the_distributions_restore() {
        // netfilter-persistent flushes the chain before it restores, so a rule applied ahead of it
        // would be wiped by it.
        assert!(firewall_unit().contains("After=netfilter-persistent.service"));
        assert!(firewall_unit().contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn free_space_is_read_from_the_nearest_existing_ancestor() {
        // The install reads this before k0s has ever created its data directory, so a path that
        // does not exist yet must still give an answer rather than none.
        let dir = tempfile::tempdir().expect("a temp dir");
        let missing = dir.path().join("k0s").join("not-created-yet");
        assert_eq!(nearest_existing(&missing), Some(dir.path()));
        assert_eq!(nearest_existing(dir.path()), Some(dir.path()));
    }

    #[test]
    fn rejects_a_device_outside_dev() {
        assert!(validate_device_path("sdb").is_err());
        assert!(validate_device_path("/mnt/disk").is_err());
        assert!(validate_device_path("/dev/sdb").is_ok());
    }

    #[test]
    fn rejects_a_device_with_whitespace() {
        assert!(validate_device_path("/dev/sdb foo").is_err());
    }

    #[test]
    fn fstab_entry_is_keyed_by_uuid_and_tolerates_a_missing_disk() {
        let line = fstab_entry("1234-abcd");
        assert!(line.starts_with("UUID=1234-abcd /var/lib/k0s ext4 "));
        assert!(line.contains("nofail"));
        assert!(line.ends_with(MANAGED_TAG));
    }

    #[test]
    fn finds_an_existing_fstab_mount_by_uuid() {
        let fstab = "# comment\nUUID=aaaa / ext4 defaults 0 1\nUUID=bbbb /var/lib/k0s ext4 defaults,nofail 0 2\n";
        assert_eq!(
            fstab_uuid_for(fstab, "/var/lib/k0s"),
            Some("bbbb".to_string())
        );
    }

    #[test]
    fn reports_a_non_uuid_fstab_source_verbatim() {
        let fstab = "/dev/sdb /var/lib/k0s ext4 defaults 0 2\n";
        assert_eq!(
            fstab_uuid_for(fstab, "/var/lib/k0s"),
            Some("/dev/sdb".to_string())
        );
    }

    #[test]
    fn finds_an_fstab_mount_listed_after_a_blank_line() {
        let fstab =
            "UUID=aaaa / ext4 defaults 0 1\n\nUUID=bbbb /var/lib/k0s ext4 defaults,nofail 0 2\n";
        assert_eq!(
            fstab_uuid_for(fstab, "/var/lib/k0s"),
            Some("bbbb".to_string())
        );
    }

    #[test]
    fn ignores_a_commented_fstab_line() {
        let fstab = "#UUID=bbbb /var/lib/k0s ext4 defaults 0 2\n";
        assert_eq!(fstab_uuid_for(fstab, "/var/lib/k0s"), None);
    }

    #[test]
    fn no_fstab_entry_when_the_mount_point_is_absent() {
        let fstab = "UUID=aaaa / ext4 defaults 0 1\n";
        assert_eq!(fstab_uuid_for(fstab, "/var/lib/k0s"), None);
    }

    #[test]
    fn reads_the_device_mounted_at_a_path() {
        let mounts = "sysfs /sys sysfs rw 0 0\n/dev/sdb /var/lib/k0s ext4 rw 0 0\n";
        assert_eq!(
            mount_source_of(mounts, "/var/lib/k0s"),
            Some("/dev/sdb".to_string())
        );
        assert_eq!(mount_source_of(mounts, "/var/lib/other"), None);
    }

    #[test]
    fn reads_where_a_device_is_mounted() {
        let mounts = "/dev/sda1 / ext4 rw 0 0\n/dev/sdb /mnt/scratch ext4 rw 0 0\n";
        assert_eq!(
            mount_target_of(mounts, "/dev/sdb"),
            Some("/mnt/scratch".to_string())
        );
        assert_eq!(mount_target_of(mounts, "/dev/sdc"), None);
    }

    #[test]
    fn parses_the_iptables_backend() {
        assert_eq!(
            parse_backend("iptables v1.8.10 (nf_tables)\n"),
            Backend::NfTables
        );
        assert_eq!(parse_backend("iptables v1.8.7 (legacy)\n"), Backend::Legacy);
        assert_eq!(parse_backend("iptables v1.4.21\n"), Backend::Unknown);
    }

    #[test]
    fn spots_a_catch_all_reject() {
        assert!(has_catch_all_reject(
            "-P INPUT ACCEPT\n-A INPUT -j REJECT --reject-with icmp-host-prohibited\n"
        ));
        assert!(has_catch_all_reject("-A FORWARD -j DROP\n"));
    }

    #[test]
    fn a_targeted_reject_is_not_a_catch_all() {
        // Rejecting one port is a deliberate policy, not a blanket block.
        assert!(!has_catch_all_reject(
            "-A INPUT -p tcp --dport 25 -j REJECT\n"
        ));
        assert!(!has_catch_all_reject(
            "-A INPUT -m state --state RELATED,ESTABLISHED -j ACCEPT\n"
        ));
        assert!(!has_catch_all_reject("-P INPUT ACCEPT\n"));
    }

    #[test]
    fn the_port_lists_fit_one_multiport_rule() {
        // Exceeding the kernel's limit fails only on a node, so assert it here.
        assert!(multiport_entries(TCP_PORTS) <= MULTIPORT_LIMIT);
        assert!(multiport_entries(UDP_PORTS) <= MULTIPORT_LIMIT);
    }

    #[test]
    fn counts_a_port_range_as_two_entries() {
        assert_eq!(multiport_entries(&["80", "443"]), 2);
        assert_eq!(multiport_entries(&["80", "30000:32767"]), 3);
    }

    #[test]
    fn opens_the_apiserver_port_a_pod_reaches_the_cluster_service_on() {
        // A pod dialling the cluster's own service IP is DNAT'd to this node's 6443, so the
        // packet arrives on INPUT. Losing this entry breaks in-cluster API access.
        assert!(TCP_PORTS.contains(&"6443"));
        assert_eq!(multiport_spec(&["80", "443"]), "80,443");
    }

    #[test]
    fn parses_available_gigabytes() {
        assert_eq!(avail_gb("Avail\n1907G\n"), Some(1907));
        assert_eq!(avail_gb("Avail\n  94G\n"), Some(94));
        assert_eq!(avail_gb("Avail\n"), None);
    }

    #[test]
    fn maps_a_sysctl_key_to_its_proc_path() {
        assert_eq!(
            sysctl_path("fs.inotify.max_user_instances"),
            "/proc/sys/fs/inotify/max_user_instances"
        );
    }

    #[test]
    fn the_drop_in_records_the_value_that_was_applied() {
        let body = sysctl_drop_in(&[("fs.inotify.max_user_instances", 8192)]);
        assert_eq!(
            body,
            "# managed by spur\nfs.inotify.max_user_instances = 8192\n"
        );
    }

    #[test]
    fn the_drop_in_keeps_a_value_already_above_the_floor() {
        // A kernel scales max_user_watches from RAM, so writing the floor back would lower it.
        let body = sysctl_drop_in(&[("fs.inotify.max_user_watches", 761911)]);
        assert!(body.contains("fs.inotify.max_user_watches = 761911"));
    }
}
