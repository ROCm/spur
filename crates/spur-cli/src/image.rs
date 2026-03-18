//! `spur image` subcommands for container image management.

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Container image management.
#[derive(Parser, Debug)]
#[command(name = "image", about = "Manage container images")]
pub struct ImageArgs {
    #[command(subcommand)]
    pub command: ImageCommand,
}

#[derive(Subcommand, Debug)]
pub enum ImageCommand {
    /// Import a Docker/OCI image as squashfs.
    ///
    /// Downloads the image and converts it to a squashfs file that can be
    /// used with --container-image in job submissions.
    Import {
        /// Image URI (e.g., "docker://nvcr.io/nvidia/pytorch:24.01", "ubuntu:22.04")
        image: String,
    },
    /// List imported images.
    List,
    /// Remove an imported image.
    Remove {
        /// Image name
        name: String,
    },
}

pub async fn main() -> Result<()> {
    let args = ImageArgs::parse();

    match args.command {
        ImageCommand::Import { image } => cmd_import(&image).await,
        ImageCommand::List => cmd_list(),
        ImageCommand::Remove { name } => cmd_remove(&name),
    }
}

async fn cmd_import(image: &str) -> Result<()> {
    let image_ref = spur_net::oci::parse_image_ref(image);
    eprintln!(
        "Importing {}:{} from {}",
        image_ref.repository, image_ref.tag, image_ref.registry
    );

    let image_dir = std::path::Path::new("/var/spool/spur/images");
    let path = spur_net::pull_image(image, image_dir).await?;

    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "Imported: {} ({:.1} MB)",
        path.display(),
        size as f64 / 1_048_576.0
    );

    Ok(())
}

fn cmd_list() -> Result<()> {
    let image_dir = std::path::Path::new("/var/spool/spur/images");
    if !image_dir.exists() {
        eprintln!("No images imported yet.");
        return Ok(());
    }

    let mut images: Vec<(String, u64)> = Vec::new();
    for entry in std::fs::read_dir(image_dir)?.flatten() {
        let path = entry.path();
        if path.extension().map_or(false, |ext| ext == "sqsh") {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            images.push((name, size));
        }
    }

    if images.is_empty() {
        eprintln!("No images imported yet.");
        return Ok(());
    }

    images.sort_by(|a, b| a.0.cmp(&b.0));

    println!("{:<50} {:>10}", "IMAGE", "SIZE");
    for (name, size) in &images {
        let display_name = name.replace('+', "/");
        let size_str = if *size > 1_073_741_824 {
            format!("{:.1} GB", *size as f64 / 1_073_741_824.0)
        } else {
            format!("{:.1} MB", *size as f64 / 1_048_576.0)
        };
        println!("{:<50} {:>10}", display_name, size_str);
    }

    Ok(())
}

fn cmd_remove(name: &str) -> Result<()> {
    let sanitized = spur_net::oci::sanitize_name(name);
    let path = format!("/var/spool/spur/images/{}.sqsh", sanitized);

    if !std::path::Path::new(&path).exists() {
        anyhow::bail!("image '{}' not found", name);
    }

    std::fs::remove_file(&path)?;
    eprintln!("Removed: {}", name);
    Ok(())
}
