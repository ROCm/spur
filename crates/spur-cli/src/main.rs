mod exec;
mod format_engine;
mod image;
mod net;
mod sacct;
mod sacctmgr;
mod salloc;
mod sattach;
mod sbatch;
mod scancel;
mod scontrol;
mod scrontab;
mod sdiag;
mod sinfo;
mod smd;
mod sprio;
mod squeue;
mod sreport;
mod srun;
mod sshare;
mod sstat;
mod strigger;

use std::path::Path;

/// If SPUR_CONTROLLER_ADDR is not already set, try to read the controller
/// address from the config file so that all subcommands pick it up
/// automatically via their `env = "SPUR_CONTROLLER_ADDR"` clap annotation.
///
/// Priority: --controller CLI arg > SPUR_CONTROLLER_ADDR env > config file > default
fn load_controller_addr_from_config() {
    if std::env::var("SPUR_CONTROLLER_ADDR").is_ok() {
        return; // User already set it explicitly
    }

    // Check SPUR_CONF for custom config path, then /etc/spur/spur.conf
    let config_path = std::env::var("SPUR_CONF")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/etc/spur/spur.conf"));

    if !config_path.exists() {
        return;
    }

    if let Ok(config) = spur_core::config::SlurmConfig::load(&config_path) {
        // Extract host and port from the config
        let host = config
            .controller
            .hosts
            .first()
            .map(|h| h.as_str())
            .unwrap_or("localhost");
        let port = config
            .controller
            .listen_addr
            .rsplit(':')
            .next()
            .unwrap_or("6817");
        let addr = format!("http://{}:{}", host, port);
        std::env::set_var("SPUR_CONTROLLER_ADDR", &addr);
    }
}

fn main() -> anyhow::Result<()> {
    // Load controller address from config file (if not set via env var)
    load_controller_addr_from_config();

    // Multi-call binary: dispatch based on argv[0] (symlink name).
    let argv0 = std::env::args().next().unwrap_or_else(|| "spur".into());
    let bin_name = Path::new(&argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("spur");

    let runtime = tokio::runtime::Runtime::new()?;

    // Slurm-compatible symlink dispatch (backward compat)
    match bin_name {
        "salloc" => return runtime.block_on(salloc::main()),
        "sbatch" => return runtime.block_on(sbatch::main()),
        "srun" => return runtime.block_on(srun::main()),
        "squeue" => return runtime.block_on(squeue::main()),
        "scancel" => return runtime.block_on(scancel::main()),
        "sinfo" => return runtime.block_on(sinfo::main()),
        "sacct" => return runtime.block_on(sacct::main()),
        "sacctmgr" => return runtime.block_on(sacctmgr::main()),
        "scontrol" => return runtime.block_on(scontrol::main()),
        "sprio" => return runtime.block_on(sprio::main()),
        "sshare" => return runtime.block_on(sshare::main()),
        "sstat" => return runtime.block_on(sstat::main()),
        "sdiag" => return runtime.block_on(sdiag::main()),
        "sreport" => return runtime.block_on(sreport::main()),
        "strigger" => return runtime.block_on(strigger::main()),
        "sattach" => return runtime.block_on(sattach::main()),
        "scrontab" => return runtime.block_on(scrontab::main()),
        "smd" => return runtime.block_on(smd::main()),
        _ => {}
    }

    // Native spur CLI: `spur <command> [args...]`
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    // Map command name to the canonical binary name used by the subcommand parser.
    // This is needed because each subcommand calls try_parse_from(std::env::args())
    // and expects argv[0] to be its own name (e.g., "squeue"), not "spur".
    // We rewrite argv so the subcommand sees ["squeue", ...remaining args...].
    let canonical = match args[1].as_str() {
        "submit" => Some("sbatch"),
        "run" => Some("srun"),
        "salloc" | "alloc" => Some("salloc"),
        "queue" | "jobs" => Some("squeue"),
        "cancel" | "kill" => Some("scancel"),
        "nodes" | "info" => Some("sinfo"),
        "history" | "acct" => Some("sacct"),
        "accounts" | "acctmgr" => Some("sacctmgr"),
        "show" | "control" | "ctl" => Some("scontrol"),
        "priority" | "prio" => Some("sprio"),
        "share" | "fairshare" => Some("sshare"),
        "stat" | "jobstat" => Some("sstat"),
        "diag" | "diagnostics" => Some("sdiag"),
        "report" | "usage" => Some("sreport"),
        "trigger" | "triggers" => Some("strigger"),
        "attach" => Some("sattach"),
        "crontab" | "cron" => Some("scrontab"),
        "health" | "monitor" => Some("smd"),
        "sbatch" | "srun" | "squeue" | "scancel" | "sinfo" | "sacct" | "sacctmgr" | "scontrol"
        | "sprio" | "sshare" | "sstat" | "sdiag" | "sreport" | "strigger" | "sattach"
        | "scrontab" | "smd" => Some(args[1].as_str()),
        "net" | "image" | "exec" => Some(args[1].as_str()),
        _ => None,
    };

    if let Some(cmd) = canonical {
        // Rewrite argv: replace ["spur", "cmd", ...rest] with ["cmd", ...rest]
        //
        // Special case (issue #53): `spur show node X` should dispatch as
        // `scontrol show node X`, not `scontrol node X`. When the user's
        // command is "show", insert the implicit "show" subcommand for scontrol.
        let implicit_show = args[1].as_str() == "show" && cmd == "scontrol";
        let rewritten: Vec<String> = std::iter::once(cmd.to_string())
            .chain(if implicit_show {
                vec!["show".to_string()]
            } else {
                vec![]
            })
            .chain(args[2..].iter().cloned())
            .collect();
        // Temporarily override process args for the subcommand parser
        std::env::set_var("SPUR_ARGV0_OVERRIDE", "1");
        let result = match cmd {
            "sbatch" | "submit" => runtime.block_on(sbatch::main_with_args(rewritten)),
            "srun" | "run" => runtime.block_on(srun::main_with_args(rewritten)),
            "salloc" | "alloc" => runtime.block_on(salloc::main_with_args(rewritten)),
            "squeue" | "queue" | "jobs" => runtime.block_on(squeue::main_with_args(rewritten)),
            "scancel" | "cancel" | "kill" => runtime.block_on(scancel::main_with_args(rewritten)),
            "sinfo" | "nodes" | "info" => runtime.block_on(sinfo::main_with_args(rewritten)),
            "sacct" | "history" | "acct" => runtime.block_on(sacct::main_with_args(rewritten)),
            "sacctmgr" | "accounts" | "acctmgr" => {
                runtime.block_on(sacctmgr::main_with_args(rewritten))
            }
            "scontrol" | "show" | "control" | "ctl" => {
                runtime.block_on(scontrol::main_with_args(rewritten))
            }
            "sprio" | "priority" | "prio" => runtime.block_on(sprio::main_with_args(rewritten)),
            "sshare" | "share" | "fairshare" => runtime.block_on(sshare::main_with_args(rewritten)),
            "sstat" | "stat" | "jobstat" => runtime.block_on(sstat::main_with_args(rewritten)),
            "sdiag" | "diag" | "diagnostics" => runtime.block_on(sdiag::main_with_args(rewritten)),
            "sreport" | "report" | "usage" => runtime.block_on(sreport::main_with_args(rewritten)),
            "strigger" | "trigger" | "triggers" => {
                runtime.block_on(strigger::main_with_args(rewritten))
            }
            "sattach" | "attach" => runtime.block_on(sattach::main_with_args(rewritten)),
            "scrontab" | "crontab" | "cron" => {
                runtime.block_on(scrontab::main_with_args(rewritten))
            }
            "smd" | "health" | "monitor" => runtime.block_on(smd::main_with_args(rewritten)),
            "net" => runtime.block_on(net::main_with_args(rewritten)),
            "image" => runtime.block_on(image::main_with_args(rewritten)),
            "exec" => runtime.block_on(exec::main_with_args(rewritten)),
            _ => unreachable!(),
        };
        return result;
    }

    match args[1].as_str() {
        "version" | "--version" | "-V" => {
            println!("spur {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("spur: unknown command '{}'", other);
            eprintln!();
            print_usage();
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!("spur — AI-native job scheduler");
    eprintln!();
    eprintln!("Usage: spur <command> [args...]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  net         Manage WireGuard mesh network (init/join/status)");
    eprintln!("  image       Manage container images (import/list/remove)");
    eprintln!("  exec        Execute a command inside a running container job");
    eprintln!("  submit      Submit a batch job script");
    eprintln!("  run         Run a parallel job (interactive)");
    eprintln!("  alloc       Allocate resources for an interactive session");
    eprintln!("  queue       View the job queue");
    eprintln!("  cancel      Cancel pending or running jobs");
    eprintln!("  nodes       View cluster node information");
    eprintln!("  history     View job accounting history");
    eprintln!("  accounts    Manage accounts, users, and QOS");
    eprintln!("  show        Show detailed job/node/partition info");
    eprintln!("  priority    View job priority breakdown");
    eprintln!("  share       Show fair-share information");
    eprintln!("  stat        Display running job statistics");
    eprintln!("  diag        Show scheduler diagnostics");
    eprintln!("  report      Generate usage reports");
    eprintln!("  trigger     Manage event triggers");
    eprintln!("  attach      Attach to a running job's I/O");
    eprintln!("  crontab     Manage recurring cron-style jobs");
    eprintln!("  health      Node health monitoring");
    eprintln!("  version     Show version");
    eprintln!();
    eprintln!("Slurm-compatible aliases (also work as symlinks):");
    eprintln!("  salloc sbatch srun squeue scancel sinfo sacct sacctmgr scontrol");
    eprintln!("  sprio sshare sstat sdiag sreport strigger sattach scrontab smd");
}
