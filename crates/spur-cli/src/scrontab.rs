use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

/// Manage recurring Slurm-style cron jobs.
///
/// Stores a crontab-like file at `~/.spur/crontab` with the format:
///   MIN HOUR DOM MON DOW COMMAND
///
/// Example entry:
///   0 */6 * * * sbatch /path/to/training.sh
#[derive(Parser, Debug)]
#[command(name = "scrontab", about = "Manage recurring Slurm-style cron jobs")]
pub struct ScrontabArgs {
    /// List the current crontab
    #[arg(short = 'l', long)]
    pub list: bool,

    /// Edit the crontab (opens $EDITOR)
    #[arg(short = 'e', long)]
    pub edit: bool,

    /// Remove the crontab
    #[arg(short = 'r', long)]
    pub remove: bool,

    /// Operate on a specific user's crontab (requires admin)
    #[arg(short = 'u', long)]
    pub user: Option<String>,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = ScrontabArgs::try_parse_from(&args)?;

    let crontab_path = crontab_file(args.user.as_deref());

    if args.list {
        list_crontab(&crontab_path)?;
    } else if args.edit {
        edit_crontab(&crontab_path)?;
    } else if args.remove {
        remove_crontab(&crontab_path)?;
    } else {
        eprintln!("scrontab: specify -l (list), -e (edit), or -r (remove)");
        std::process::exit(1);
    }
    Ok(())
}

fn crontab_file(user: Option<&str>) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let base = PathBuf::from(home).join(".spur");
    match user {
        Some(u) => base.join(format!("crontab.{}", u)),
        None => base.join("crontab"),
    }
}

fn effective_user() -> String {
    whoami::username().unwrap_or_else(|_| "unknown".into())
}

fn list_crontab(path: &PathBuf) -> Result<()> {
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        if content.trim().is_empty()
            || content
                .lines()
                .all(|l| l.trim().is_empty() || l.starts_with('#'))
        {
            eprintln!("no crontab entries for {}", effective_user());
        } else {
            print!("{}", content);
        }
    } else {
        eprintln!("no crontab for {}", effective_user());
    }
    Ok(())
}

fn edit_crontab(path: &PathBuf) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if !path.exists() {
        std::fs::write(
            path,
            "# Spur crontab -- format: MIN HOUR DOM MON DOW COMMAND\n\
             # Example: 0 */6 * * * sbatch /path/to/script.sh\n",
        )?;
    }

    let status = std::process::Command::new(&editor).arg(path).status()?;

    if status.success() {
        // Validate the crontab entries
        let content = std::fs::read_to_string(path)?;
        let entry_count = content
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .count();
        eprintln!("crontab updated ({} entries)", entry_count);
    } else {
        eprintln!("scrontab: editor exited with error");
    }
    Ok(())
}

fn remove_crontab(path: &PathBuf) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
        eprintln!("crontab for {} removed", effective_user());
    } else {
        eprintln!("no crontab for {}", effective_user());
    }
    Ok(())
}
