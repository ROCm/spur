use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

/// Manage event triggers (Slurm-compatible strigger).
///
/// Triggers execute a program when a specified event occurs (e.g., node down,
/// job end).  For simplicity, triggers are stored locally in
/// `~/.spur/triggers.json`.
#[derive(Parser, Debug)]
#[command(name = "strigger", about = "Manage event triggers")]
pub struct StriggerArgs {
    /// Create a new trigger
    #[arg(long)]
    pub set: bool,

    /// List existing triggers
    #[arg(long)]
    pub get: bool,

    /// Delete triggers
    #[arg(long)]
    pub clear: bool,

    /// Job ID to associate with trigger
    #[arg(long)]
    pub jobid: Option<u32>,

    /// Node name to associate with trigger
    #[arg(long)]
    pub node: Option<String>,

    /// Event type: node_down, node_up, job_end, job_fail, time
    #[arg(long = "type", name = "type")]
    pub trigger_type: Option<String>,

    /// Program to execute when the trigger fires
    #[arg(long)]
    pub program: Option<String>,

    /// Seconds before/after event
    #[arg(long, default_value = "0")]
    pub offset: i32,

    /// Controller address (unused for local triggers, reserved for future)
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    pub controller: String,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = StriggerArgs::try_parse_from(&args)?;

    if args.set {
        set_trigger(&args)?;
    } else if args.get {
        list_triggers()?;
    } else if args.clear {
        clear_triggers(&args)?;
    } else {
        eprintln!("strigger: specify --set, --get, or --clear");
        std::process::exit(1);
    }
    Ok(())
}

fn triggers_file() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".spur/triggers.json")
}

fn load_triggers() -> Result<Vec<serde_json::Value>> {
    let path = triggers_file();
    if path.exists() {
        let data = std::fs::read_to_string(&path)?;
        let triggers: Vec<serde_json::Value> = serde_json::from_str(&data)?;
        Ok(triggers)
    } else {
        Ok(Vec::new())
    }
}

fn save_triggers(triggers: &[serde_json::Value]) -> Result<()> {
    let path = triggers_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(triggers)?)?;
    Ok(())
}

const VALID_TYPES: &[&str] = &["node_down", "node_up", "job_end", "job_fail", "time"];

fn set_trigger(args: &StriggerArgs) -> Result<()> {
    let trigger_type = args.trigger_type.as_deref().unwrap_or("job_end");
    if !VALID_TYPES.contains(&trigger_type) {
        anyhow::bail!(
            "strigger: invalid trigger type '{}'. Valid types: {}",
            trigger_type,
            VALID_TYPES.join(", ")
        );
    }

    let program = match args.program.as_deref() {
        Some(p) if !p.is_empty() => p,
        _ => anyhow::bail!("strigger: --program is required with --set"),
    };

    let trigger = serde_json::json!({
        "type": trigger_type,
        "program": program,
        "job_id": args.jobid,
        "node": args.node,
        "offset": args.offset,
    });

    let mut triggers = load_triggers()?;
    triggers.push(trigger);
    save_triggers(&triggers)?;

    eprintln!("Trigger set: {} -> {}", trigger_type, program);
    Ok(())
}

fn list_triggers() -> Result<()> {
    let triggers = load_triggers()?;
    if triggers.is_empty() {
        println!("No triggers set");
        return Ok(());
    }

    println!(
        "{:<10} {:<15} {:<10} {:<10} {}",
        "TRIG_ID", "TYPE", "JOB_ID", "NODE", "PROGRAM"
    );
    for (i, t) in triggers.iter().enumerate() {
        println!(
            "{:<10} {:<15} {:<10} {:<10} {}",
            i,
            t["type"].as_str().unwrap_or(""),
            t["job_id"]
                .as_u64()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            t["node"].as_str().unwrap_or("-"),
            t["program"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

fn clear_triggers(args: &StriggerArgs) -> Result<()> {
    let path = triggers_file();
    if !path.exists() {
        eprintln!("No triggers to clear");
        return Ok(());
    }

    if let Some(job_id) = args.jobid {
        let mut triggers = load_triggers()?;
        let before = triggers.len();
        triggers.retain(|t| t["job_id"].as_u64() != Some(job_id as u64));
        let removed = before - triggers.len();
        save_triggers(&triggers)?;
        eprintln!("Cleared {} trigger(s) for job {}", removed, job_id);
    } else if let Some(ref node) = args.node {
        let mut triggers = load_triggers()?;
        let before = triggers.len();
        triggers.retain(|t| t["node"].as_str() != Some(node.as_str()));
        let removed = before - triggers.len();
        save_triggers(&triggers)?;
        eprintln!("Cleared {} trigger(s) for node {}", removed, node);
    } else {
        std::fs::remove_file(&path)?;
        eprintln!("All triggers cleared");
    }
    Ok(())
}
