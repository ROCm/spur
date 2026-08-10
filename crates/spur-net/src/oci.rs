// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Native OCI/Docker image puller.
//!
//! Downloads container images directly from registries using the
//! Docker Registry HTTP API v2. No dependency on Docker, skopeo,
//! umoci, or enroot.
//!
//! Flow:
//! 1. Parse image reference (registry/repo:tag)
//! 2. Authenticate (token-based for Docker Hub, anonymous for others)
//! 3. Fetch manifest → list of layer digests
//! 4. Download each layer blob
//! 5. Extract layers in order to build rootfs
//! 6. Pack rootfs into squashfs via mksquashfs

use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::Deserialize;
use tracing::{debug, info};

/// A parsed container image reference.
#[derive(Debug, Clone)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

/// Docker Registry auth token response.
#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

/// OCI/Docker manifest (simplified — handles both v2s2 and OCI).
#[derive(Deserialize)]
struct Manifest {
    config: ConfigDescriptor,
    #[serde(default)]
    layers: Vec<LayerDescriptor>,
    // v1 compat: some registries return "fsLayers" instead
}

#[derive(Deserialize)]
struct ConfigDescriptor {
    digest: String,
}

#[derive(Deserialize)]
struct ImageConfiguration {
    architecture: String,
    os: String,
}

#[derive(Deserialize)]
struct ManifestList {
    manifests: Vec<ManifestEntry>,
}

#[derive(Deserialize)]
struct ManifestEntry {
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}

#[derive(Deserialize)]
struct LayerDescriptor {
    digest: String,
    size: u64,
    #[serde(rename = "mediaType")]
    media_type: String,
}

/// Parse an image reference into registry, repository, and tag.
///
/// Examples:
/// - `ubuntu:22.04` → `docker.io`, `library/ubuntu`, `22.04`
/// - `nvcr.io/nvidia/pytorch:24.01` → `nvcr.io`, `nvidia/pytorch`, `24.01`
/// - `docker://ubuntu` → `docker.io`, `library/ubuntu`, `latest`
/// - `ghcr.io/org/repo` → `ghcr.io`, `org/repo`, `latest`
pub fn parse_image_ref(image: &str) -> ImageRef {
    let image = image.strip_prefix("docker://").unwrap_or(image);

    let (name, tag) = if let Some((n, t)) = image.rsplit_once(':') {
        // Make sure the ':' is for the tag, not a port
        if t.contains('/') {
            (image, "latest")
        } else {
            (n, t)
        }
    } else {
        (image, "latest")
    };

    let (registry, repository) =
        if name.contains('.') || name.contains(':') || name.contains("localhost") {
            // Has a dot or colon → explicit registry
            if let Some((reg, repo)) = name.split_once('/') {
                (reg.to_string(), repo.to_string())
            } else {
                ("docker.io".to_string(), format!("library/{}", name))
            }
        } else if name.contains('/') {
            // user/repo format → Docker Hub
            ("docker.io".to_string(), name.to_string())
        } else {
            // bare name → Docker Hub official library
            ("docker.io".to_string(), format!("library/{}", name))
        };

    ImageRef {
        registry,
        repository,
        tag: tag.to_string(),
    }
}

impl ImageRef {
    /// Canonical `registry/repository:tag` form.
    ///
    /// Equivalent references (`busybox`, `busybox:latest`, `docker://busybox`,
    /// `docker.io/library/busybox:latest`) all normalize to the same string.
    pub fn canonical(&self) -> String {
        format!("{}/{}:{}", self.registry, self.repository, self.tag)
    }
}

/// Canonical filename stem for an image reference.
///
/// Derives the on-disk name from the normalized `ImageRef` rather than the raw
/// input string, so all equivalent references map to a single stored image.
pub fn image_file_stem(image: &str) -> String {
    sanitize_name(&parse_image_ref(image).canonical())
}

/// Render a stored filename stem back to a canonical image reference for display.
///
/// The last `+` is always the tag separator (canonical form guarantees a tag),
/// and remaining `+` map back to `/`. Port-bearing registries lose the port
/// colon (shown as `/`) since `sanitize_name` maps both `:` and `/` to `+`.
pub fn display_name(stem: &str) -> String {
    match stem.rsplit_once('+') {
        Some((path, tag)) => format!("{}:{}", path.replace('+', "/"), tag),
        None => stem.to_string(),
    }
}

/// Pull an image from a registry and create a squashfs file.
///
/// Returns the path to the squashfs file.
pub async fn pull_image(image: &str, output_dir: &Path, arch: &str) -> anyhow::Result<PathBuf> {
    let image_ref = parse_image_ref(image);
    info!(
        registry = %image_ref.registry,
        repository = %image_ref.repository,
        tag = %image_ref.tag,
        architecture = arch,
        "pulling image"
    );

    let sanitized = sanitize_name(&image_ref.canonical());
    let sqsh_path = output_dir.join(format!("{}.sqsh", sanitized));
    let arch_path = sqsh_path.with_extension("sqsh.arch");

    std::fs::create_dir_all(output_dir)?;
    {
        let _cache_lock = lock_image_cache(output_dir, &sanitized)?;
        if cached_image_matches(&sqsh_path, &arch_path, arch) {
            info!(path = %sqsh_path.display(), architecture = arch, "image already exists");
            return Ok(sqsh_path);
        }
        if sqsh_path.exists() {
            info!(path = %sqsh_path.display(), architecture = arch, "replacing image for requested architecture");
        }
    }

    // Each pull owns its working tree so concurrent imports cannot remove it.
    let tmp_dir = pull_staging_dir(output_dir, &sanitized);
    let rootfs_dir = tmp_dir.join("rootfs");
    let staged_sqsh_path = tmp_dir.join("image.sqsh");
    let staged_arch_path = tmp_dir.join("image.sqsh.arch");
    std::fs::create_dir_all(&rootfs_dir)?;

    let result = pull_and_extract(&image_ref, &rootfs_dir, arch).await;
    if let Err(e) = &result {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(anyhow::anyhow!("{}", e));
    }

    // Pack into squashfs
    info!("creating squashfs image");
    let mksquashfs_result = std::process::Command::new("mksquashfs")
        .arg(&rootfs_dir)
        .arg(&staged_sqsh_path)
        .args(["-noappend", "-comp", "zstd", "-quiet"])
        .output();

    match mksquashfs_result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_dir_all(&tmp_dir);
            bail!("mksquashfs failed: {}", stderr.trim());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            bail!(
                "mksquashfs not found. Install squashfs-tools:\n  \
                 sudo apt install squashfs-tools    # Debian/Ubuntu\n  \
                 sudo dnf install squashfs-tools    # Fedora/RHEL"
            );
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            bail!("failed to run mksquashfs: {}", e);
        }
    }

    let finalize_result = (|| -> anyhow::Result<bool> {
        std::fs::write(&staged_arch_path, oci_architecture(arch))?;
        let _cache_lock = lock_image_cache(output_dir, &sanitized)?;
        if cached_image_matches(&sqsh_path, &arch_path, arch) {
            return Ok(false);
        }
        install_staged_image(&staged_sqsh_path, &staged_arch_path, &sqsh_path, &arch_path)?;
        Ok(true)
    })();
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let installed = finalize_result?;

    if !installed {
        info!(path = %sqsh_path.display(), architecture = arch, "image already installed by another pull");
        return Ok(sqsh_path);
    }

    let size = std::fs::metadata(&sqsh_path).map(|m| m.len()).unwrap_or(0);
    info!(
        path = %sqsh_path.display(),
        size_mb = size / 1_048_576,
        "image pulled successfully"
    );

    Ok(sqsh_path)
}

/// Download manifest and layers, extract to rootfs directory.
async fn pull_and_extract(
    image_ref: &ImageRef,
    rootfs_dir: &Path,
    arch: &str,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder().user_agent("spur/0.1").build()?;

    // Get auth token
    let token = get_auth_token(&client, image_ref).await?;

    // Fetch manifest
    let registry_url = registry_base_url(&image_ref.registry);
    let manifest_url = format!(
        "{}/v2/{}/manifests/{}",
        registry_url, image_ref.repository, image_ref.tag
    );

    debug!(url = %manifest_url, "fetching manifest");
    let mut req = client.get(&manifest_url).header(
        ACCEPT,
        "application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.index.v1+json, \
         application/vnd.docker.distribution.manifest.list.v2+json",
    );
    if let Some(ref token) = token {
        req = req.header(AUTHORIZATION, format!("Bearer {}", token));
    }

    let resp = req.send().await.context("failed to fetch manifest")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "registry returned {} for manifest of {}:{}\n{}",
            status,
            image_ref.repository,
            image_ref.tag,
            body.chars().take(500).collect::<String>()
        );
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let manifest_body = resp.text().await?;

    // Handle manifest list / image index (multi-arch)
    let manifest: Manifest =
        if content_type.contains("manifest.list") || content_type.contains("image.index") {
            let index = resolve_manifest_list(
                &client,
                &manifest_body,
                &registry_url,
                image_ref,
                token.as_deref(),
                arch,
            )
            .await?;
            index
        } else {
            let manifest: Manifest =
                serde_json::from_str(&manifest_body).context("failed to parse manifest JSON")?;
            verify_direct_manifest_platform(
                &client,
                &manifest,
                &registry_url,
                image_ref,
                token.as_deref(),
                arch,
            )
            .await?;
            manifest
        };

    if manifest.layers.is_empty() {
        bail!("manifest has no layers — image may be empty or unsupported format");
    }

    info!(layers = manifest.layers.len(), "downloading layers");

    // Layer cache directory
    let cache_dir = PathBuf::from(
        std::env::var("SPUR_IMAGE_CACHE")
            .unwrap_or_else(|_| "/var/spool/spur/images/.layers".into()),
    );
    let _ = std::fs::create_dir_all(&cache_dir);

    // Download layers in parallel, then extract sequentially (order matters)
    let mut layer_data: Vec<(usize, bytes::Bytes)> = Vec::new();

    // Parallel download
    let mut handles = Vec::new();
    for (i, layer) in manifest.layers.iter().enumerate() {
        let digest = layer.digest.clone();
        let size = layer.size;
        let cache_path = cache_dir.join(digest.replace(':', "_"));

        // Check layer cache
        if cache_path.exists() {
            if let Ok(cached) = std::fs::read(&cache_path) {
                info!(
                    layer = i + 1,
                    total = manifest.layers.len(),
                    digest = %digest,
                    "layer cached, skipping download"
                );
                layer_data.push((i, bytes::Bytes::from(cached)));
                continue;
            }
        }

        let blob_url = format!(
            "{}/v2/{}/blobs/{}",
            registry_url, image_ref.repository, digest
        );
        let client = client.clone();
        let token = token.clone();

        let handle = tokio::spawn(async move {
            info!(
                layer = i + 1,
                digest = %digest,
                size_mb = size / 1_048_576,
                "downloading layer"
            );

            let mut req = client.get(&blob_url);
            if let Some(ref token) = token {
                req = req.header(AUTHORIZATION, format!("Bearer {}", token));
            }

            let resp = req.send().await.context("failed to download layer")?;
            if !resp.status().is_success() {
                bail!("registry returned {} for layer {}", resp.status(), digest);
            }

            let data = resp.bytes().await.context("failed to read layer body")?;

            // Cache the layer
            let _ = std::fs::write(&cache_path, &data);

            Ok::<(usize, bytes::Bytes), anyhow::Error>((i, data))
        });
        handles.push(handle);
    }

    // Collect parallel downloads
    for handle in handles {
        let (idx, data) = handle.await.context("layer download task panicked")??;
        layer_data.push((idx, data));
    }

    // Sort by layer index (parallel downloads may complete out of order)
    layer_data.sort_by_key(|(idx, _)| *idx);

    // Extract layers sequentially (order matters for whiteout files)
    for (i, (_, data)) in layer_data.iter().enumerate() {
        let media_type = &manifest.layers[i].media_type;
        extract_layer(data, Some(media_type), rootfs_dir)
            .with_context(|| format!("failed to extract layer {}", i + 1))?;
    }

    Ok(())
}

/// Registry credentials loaded from file or environment.
#[derive(Debug, Clone)]
pub struct RegistryCredentials {
    pub username: String,
    pub password: String,
}

/// Load credentials for a registry from:
/// 1. Environment: SPUR_REGISTRY_USER + SPUR_REGISTRY_PASSWORD
/// 2. Credentials file: ~/.config/spur/credentials (netrc format)
/// 3. Docker config: ~/.docker/config.json (for compat)
pub fn load_credentials(registry: &str) -> Option<RegistryCredentials> {
    // 1. Environment variables
    if let (Ok(user), Ok(pass)) = (
        std::env::var("SPUR_REGISTRY_USER"),
        std::env::var("SPUR_REGISTRY_PASSWORD"),
    ) {
        if !user.is_empty() {
            return Some(RegistryCredentials {
                username: user,
                password: pass,
            });
        }
    }

    // 2. Spur credentials file (netrc format: machine <registry> login <user> password <pass>)
    let cred_path = dirs_credentials_path();
    if let Ok(content) = std::fs::read_to_string(&cred_path) {
        if let Some(cred) = parse_netrc(&content, registry) {
            return Some(cred);
        }
    }

    // 3. Docker config.json (base64 encoded "user:pass" in auths)
    if let Some(cred) = load_docker_config_auth(registry) {
        return Some(cred);
    }

    None
}

fn dirs_credentials_path() -> PathBuf {
    if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("spur/credentials")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/spur/credentials")
    } else {
        PathBuf::from("/etc/spur/credentials")
    }
}

fn parse_netrc(content: &str, registry: &str) -> Option<RegistryCredentials> {
    let mut machine_match = false;
    let mut username = None;
    let mut password = None;
    let tokens: Vec<&str> = content.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "machine" if i + 1 < tokens.len() => {
                machine_match = tokens[i + 1] == registry
                    || (registry == "docker.io" && tokens[i + 1] == "registry-1.docker.io");
                username = None;
                password = None;
                i += 2;
            }
            "login" if machine_match && i + 1 < tokens.len() => {
                username = Some(tokens[i + 1].to_string());
                i += 2;
            }
            "password" if machine_match && i + 1 < tokens.len() => {
                password = Some(tokens[i + 1].to_string());
                i += 2;
            }
            _ => i += 1,
        }
        if machine_match {
            if let (Some(u), Some(p)) = (&username, &password) {
                return Some(RegistryCredentials {
                    username: u.clone(),
                    password: p.clone(),
                });
            }
        }
    }
    None
}

/// Decode the `auth` field from Docker `config.json` (standard Base64 of `user:password`).
fn decode_registry_auth_b64(s: &str) -> Option<String> {
    let bytes = STANDARD.decode(s.trim()).ok()?;
    String::from_utf8(bytes).ok()
}

fn load_docker_config_auth(registry: &str) -> Option<RegistryCredentials> {
    let docker_config = if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".docker/config.json")
    } else {
        return None;
    };

    let content = std::fs::read_to_string(&docker_config).ok()?;
    let config: serde_json::Value = serde_json::from_str(&content).ok()?;
    let auths = config.get("auths")?;

    // Try exact match and common aliases
    let keys_to_try = if registry == "docker.io" {
        vec![
            "docker.io",
            "https://index.docker.io/v1/",
            "registry-1.docker.io",
        ]
    } else {
        vec![registry]
    };

    for key in keys_to_try {
        if let Some(entry) = auths.get(key) {
            if let Some(auth_b64) = entry.get("auth").and_then(|a| a.as_str()) {
                let decoded = decode_registry_auth_b64(auth_b64)?;
                let (user, pass) = decoded.split_once(':')?;
                return Some(RegistryCredentials {
                    username: user.to_string(),
                    password: pass.to_string(),
                });
            }
        }
    }

    None
}

/// Get an auth token from the registry.
///
/// Supports:
/// - Docker Hub token auth
/// - Basic auth with credentials from file/env
/// - Anonymous access for public images
async fn get_auth_token(
    client: &reqwest::Client,
    image_ref: &ImageRef,
) -> anyhow::Result<Option<String>> {
    let creds = load_credentials(&image_ref.registry);

    if image_ref.registry == "docker.io" {
        let url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            image_ref.repository
        );
        let mut req = client.get(&url);
        if let Some(ref creds) = creds {
            req = req.basic_auth(&creds.username, Some(&creds.password));
        }
        let resp = req
            .send()
            .await
            .context("failed to get Docker Hub auth token")?;
        if resp.status().is_success() {
            let token_resp: TokenResponse = resp.json().await?;
            return Ok(Some(token_resp.token));
        }
    }

    // For non-Docker Hub registries with credentials, use basic auth
    // The token will be passed as-is (basic auth encoded)
    if let Some(creds) = creds {
        use std::fmt::Write;
        let mut basic = String::new();
        write!(
            basic,
            "Basic {}",
            STANDARD.encode(format!("{}:{}", creds.username, creds.password))
        )
        .ok();
        return Ok(Some(basic));
    }

    // Try anonymous access
    Ok(None)
}

fn oci_architecture(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        arch => arch,
    }
}

fn cached_architecture_matches(cached_arch: Option<&str>, requested_arch: &str) -> bool {
    cached_arch.is_some_and(|arch| arch.trim() == oci_architecture(requested_arch))
}

fn cached_image_matches(sqsh_path: &Path, arch_path: &Path, requested_arch: &str) -> bool {
    sqsh_path.exists()
        && cached_architecture_matches(
            std::fs::read_to_string(arch_path).ok().as_deref(),
            requested_arch,
        )
}

fn pull_staging_dir(output_dir: &Path, sanitized: &str) -> PathBuf {
    output_dir.join(format!(".pulling_{}_{}", sanitized, uuid::Uuid::new_v4()))
}

fn image_cache_lock_path(output_dir: &Path, sanitized: &str) -> PathBuf {
    output_dir.join(format!(".pulling_{}.lock", sanitized))
}

fn open_image_cache_lock(output_dir: &Path, sanitized: &str) -> anyhow::Result<File> {
    let path = image_cache_lock_path(output_dir, sanitized);
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open image cache lock at {}", path.display()))
}

fn lock_image_cache(output_dir: &Path, sanitized: &str) -> anyhow::Result<File> {
    let lock = open_image_cache_lock(output_dir, sanitized)?;
    lock.lock().with_context(|| {
        format!(
            "failed to lock image cache at {}",
            image_cache_lock_path(output_dir, sanitized).display()
        )
    })?;
    Ok(lock)
}

fn install_staged_image(
    staged_sqsh_path: &Path,
    staged_arch_path: &Path,
    sqsh_path: &Path,
    arch_path: &Path,
) -> anyhow::Result<()> {
    if sqsh_path.exists() {
        match std::fs::remove_file(arch_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to invalidate image architecture at {}",
                        arch_path.display()
                    )
                });
            }
        }
        std::fs::rename(staged_sqsh_path, sqsh_path)
            .with_context(|| format!("failed to install image at {}", sqsh_path.display()))?;
        std::fs::rename(staged_arch_path, arch_path).with_context(|| {
            format!(
                "failed to record image architecture at {}",
                arch_path.display()
            )
        })?;
        return Ok(());
    }

    std::fs::rename(staged_arch_path, arch_path).with_context(|| {
        format!(
            "failed to record image architecture at {}",
            arch_path.display()
        )
    })?;
    if let Err(error) = std::fs::rename(staged_sqsh_path, sqsh_path) {
        let _ = std::fs::remove_file(arch_path);
        return Err(error)
            .with_context(|| format!("failed to install image at {}", sqsh_path.display()));
    }
    Ok(())
}

fn validate_image_configuration(body: &str, arch: &str) -> anyhow::Result<()> {
    let config: ImageConfiguration =
        serde_json::from_str(body).context("failed to parse image config JSON")?;
    let expected_arch = oci_architecture(arch);
    if config.os != "linux" || config.architecture != expected_arch {
        bail!(
            "direct manifest platform {}/{} does not match requested linux/{}",
            config.os,
            config.architecture,
            expected_arch
        );
    }
    Ok(())
}

async fn verify_direct_manifest_platform(
    client: &reqwest::Client,
    manifest: &Manifest,
    registry_url: &str,
    image_ref: &ImageRef,
    token: Option<&str>,
    arch: &str,
) -> anyhow::Result<()> {
    let url = format!(
        "{}/v2/{}/blobs/{}",
        registry_url, image_ref.repository, manifest.config.digest
    );
    let mut req = client.get(&url);
    if let Some(token) = token {
        req = req.header(AUTHORIZATION, format!("Bearer {}", token));
    }

    let resp = req
        .send()
        .await
        .context("failed to fetch direct manifest image config")?;
    if !resp.status().is_success() {
        bail!(
            "registry returned {} for image config {}",
            resp.status(),
            manifest.config.digest
        );
    }

    let body = resp
        .text()
        .await
        .context("failed to read direct manifest image config")?;
    validate_image_configuration(&body, arch)
}

fn manifest_digest_for_arch(body: &str, arch: &str) -> anyhow::Result<String> {
    let list: ManifestList = serde_json::from_str(body).context("failed to parse manifest list")?;
    let oci_arch = oci_architecture(arch);

    list.manifests
        .iter()
        .find(|manifest| {
            manifest
                .platform
                .as_ref()
                .is_some_and(|platform| platform.architecture == oci_arch && platform.os == "linux")
        })
        .map(|manifest| manifest.digest.clone())
        .ok_or_else(|| anyhow::anyhow!("no linux/{arch} manifest found in manifest list"))
}

/// Resolve a manifest list (multi-arch) to a single Linux platform manifest.
async fn resolve_manifest_list(
    client: &reqwest::Client,
    body: &str,
    registry_url: &str,
    image_ref: &ImageRef,
    token: Option<&str>,
    arch: &str,
) -> anyhow::Result<Manifest> {
    let digest = manifest_digest_for_arch(body, arch)?;

    debug!(digest = %digest, architecture = arch, "resolved manifest list to platform manifest");

    let url = format!(
        "{}/v2/{}/manifests/{}",
        registry_url, image_ref.repository, digest
    );
    let mut req = client.get(&url).header(
        ACCEPT,
        "application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.v2+json",
    );
    if let Some(token) = token {
        req = req.header(AUTHORIZATION, format!("Bearer {}", token));
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        bail!("failed to fetch platform manifest: {}", resp.status());
    }

    let manifest: Manifest = resp
        .json()
        .await
        .context("failed to parse platform manifest")?;
    Ok(manifest)
}

fn extract_layer(data: &[u8], media_type: Option<&str>, dest: &Path) -> anyhow::Result<()> {
    extract_tar(crate::image_layer::decode(data, media_type)?, dest)
}

fn extract_tar(reader: impl Read, dest: &Path) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_overwrite(true);
    // Unpack, ignoring permission errors (common in rootless)
    for entry in archive.entries()? {
        let mut entry = entry?;
        // Skip whiteout files (.wh.*) — used for layer deletion
        let path = entry.path()?.to_path_buf();
        let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
        if filename.starts_with(".wh.") {
            // Whiteout: delete the corresponding file
            let target = if filename == ".wh..wh..opq" {
                // Opaque whiteout: directory should be empty
                // (skip for now — complex to handle)
                continue;
            } else {
                let real_name = filename.strip_prefix(".wh.").unwrap_or(filename);
                dest.join(path.parent().unwrap_or(Path::new("")))
                    .join(real_name)
            };
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::remove_dir_all(&target);
            continue;
        }

        if let Err(e) = entry.unpack_in(dest) {
            // Ignore permission errors on special files
            debug!(path = %path.display(), error = %e, "skipping entry");
        }
    }
    Ok(())
}

/// Get the base URL for a registry.
fn registry_base_url(registry: &str) -> String {
    if registry == "docker.io" {
        "https://registry-1.docker.io".to_string()
    } else if registry.starts_with("localhost") {
        format!("http://{}", registry)
    } else {
        format!("https://{}", registry)
    }
}

/// Sanitize an image name for use as a filename.
pub fn sanitize_name(name: &str) -> String {
    name.replace("docker://", "").replace(['/', ':'], "+")
}

#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use flate2::{write::GzEncoder, Compression};

    use super::*;

    const MULTI_ARCH_MANIFEST: &str = r#"{
        "manifests": [
            {
                "digest": "sha256:amd64",
                "platform": { "architecture": "amd64", "os": "linux" }
            },
            {
                "digest": "sha256:arm64",
                "platform": { "architecture": "arm64", "os": "linux" }
            }
        ]
    }"#;

    fn tar_layer(path: &str, contents: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, path, contents).unwrap();
        archive.finish().unwrap();
        drop(archive);
        data
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn extract_layer_supports_uncompressed_tar() {
        let rootfs = tempfile::tempdir().unwrap();
        let layer = tar_layer("plain.txt", b"plain layer");

        extract_layer(
            &layer,
            Some("application/vnd.oci.image.layer.v1.tar"),
            rootfs.path(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(rootfs.path().join("plain.txt")).unwrap(),
            b"plain layer"
        );
    }

    #[test]
    fn extract_layer_supports_gzip_tar() {
        let rootfs = tempfile::tempdir().unwrap();
        let layer = gzip(&tar_layer("gzip.txt", b"gzip layer"));

        extract_layer(
            &layer,
            Some("application/vnd.oci.image.layer.v1.tar+gzip"),
            rootfs.path(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(rootfs.path().join("gzip.txt")).unwrap(),
            b"gzip layer"
        );
    }

    #[test]
    fn extract_layer_supports_zstd_tar() {
        let rootfs = tempfile::tempdir().unwrap();
        let layer =
            zstd::stream::encode_all(tar_layer("zstd.txt", b"zstd layer").as_slice(), 0).unwrap();

        extract_layer(
            &layer,
            Some("application/vnd.oci.image.layer.v1.tar+zstd"),
            rootfs.path(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(rootfs.path().join("zstd.txt")).unwrap(),
            b"zstd layer"
        );
    }

    #[test]
    fn extract_layer_applies_whiteout() {
        let rootfs = tempfile::tempdir().unwrap();
        let removed = rootfs.path().join("nested/removed.txt");
        let retained = rootfs.path().join("nested/retained.txt");
        std::fs::create_dir_all(removed.parent().unwrap()).unwrap();
        std::fs::write(&removed, b"remove me").unwrap();
        std::fs::write(&retained, b"keep me").unwrap();
        let layer = tar_layer("nested/.wh.removed.txt", b"");

        extract_layer(
            &layer,
            Some("application/vnd.oci.image.layer.v1.tar"),
            rootfs.path(),
        )
        .unwrap();

        assert!(!removed.exists());
        assert_eq!(std::fs::read(retained).unwrap(), b"keep me");
        assert!(!rootfs.path().join("nested/.wh.removed.txt").exists());
    }

    #[test]
    fn extract_layer_rejects_truncated_compressed_tar() {
        let rootfs = tempfile::tempdir().unwrap();
        let contents: Vec<u8> = (0..65_536)
            .scan(0x1234_5678_u32, |state, _| {
                *state ^= *state << 13;
                *state ^= *state >> 17;
                *state ^= *state << 5;
                Some(*state as u8)
            })
            .collect();
        let mut layer =
            zstd::stream::encode_all(tar_layer("data.bin", &contents).as_slice(), 0).unwrap();
        layer.truncate(layer.len() / 2);

        assert!(extract_layer(
            &layer,
            Some("application/vnd.oci.image.layer.v1.tar+zstd"),
            rootfs.path(),
        )
        .is_err());
    }

    #[test]
    fn test_decode_registry_auth_b64_valid() {
        // echo -n 'alice:secret' | base64 -w0
        let decoded = super::decode_registry_auth_b64("YWxpY2U6c2VjcmV0").expect("decode");
        assert_eq!(decoded, "alice:secret");
        let (u, p) = decoded.split_once(':').unwrap();
        assert_eq!(u, "alice");
        assert_eq!(p, "secret");
    }

    #[test]
    fn test_decode_registry_auth_b64_trims_whitespace() {
        assert_eq!(
            super::decode_registry_auth_b64("  YWxpY2U6c2VjcmV0  ").as_deref(),
            Some("alice:secret")
        );
    }

    #[test]
    fn test_decode_registry_auth_b64_invalid_characters() {
        assert!(super::decode_registry_auth_b64("YWxpY2U6c2VjcmV0!!!").is_none());
    }

    #[test]
    fn test_decode_registry_auth_b64_truncated() {
        assert!(super::decode_registry_auth_b64("YWxpY2U6c2V").is_none());
    }

    #[test]
    fn test_decode_registry_auth_b64_rejects_nonstandard_alphabet() {
        assert!(super::decode_registry_auth_b64("Y_WxpY2U6c2VjcmV0").is_none());
    }

    #[test]
    fn test_registry_auth_b64_roundtrip() {
        let cred = "myuser:mypassword";
        let enc = STANDARD.encode(cred);
        assert_eq!(super::decode_registry_auth_b64(&enc).as_deref(), Some(cred));
    }

    #[test]
    fn test_manifest_digest_for_requested_arch() {
        assert_eq!(
            manifest_digest_for_arch(MULTI_ARCH_MANIFEST, "arm64").unwrap(),
            "sha256:arm64"
        );
    }

    #[test]
    fn test_manifest_digest_normalizes_rust_arch_names() {
        assert_eq!(
            manifest_digest_for_arch(MULTI_ARCH_MANIFEST, "x86_64").unwrap(),
            "sha256:amd64"
        );
        assert_eq!(
            manifest_digest_for_arch(MULTI_ARCH_MANIFEST, "aarch64").unwrap(),
            "sha256:arm64"
        );
    }

    #[test]
    fn test_cached_architecture_must_match_requested_arch() {
        assert!(cached_architecture_matches(Some("arm64\n"), "aarch64"));
        assert!(!cached_architecture_matches(Some("amd64"), "arm64"));
        assert!(!cached_architecture_matches(None, "arm64"));
    }

    #[test]
    fn concurrent_pulls_use_distinct_staging_directories() {
        let output_dir = Path::new("/var/spool/spur/images");
        let first = pull_staging_dir(output_dir, "docker.io+library+ubuntu+latest");
        let second = pull_staging_dir(output_dir, "docker.io+library+ubuntu+latest");

        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(output_dir));
        assert!(first.file_name().is_some_and(|name| name
            .to_string_lossy()
            .starts_with(".pulling_docker.io+library+ubuntu+latest_")));
    }

    #[test]
    fn first_install_publishes_payload_and_architecture() {
        let dir = tempfile::tempdir().unwrap();
        let staged_sqsh = dir.path().join("staged.sqsh");
        let staged_arch = dir.path().join("staged.sqsh.arch");
        let sqsh = dir.path().join("image.sqsh");
        let arch = dir.path().join("image.sqsh.arch");
        std::fs::write(&staged_sqsh, b"payload").unwrap();
        std::fs::write(&staged_arch, b"arm64").unwrap();

        install_staged_image(&staged_sqsh, &staged_arch, &sqsh, &arch).unwrap();

        assert_eq!(std::fs::read(sqsh).unwrap(), b"payload");
        assert_eq!(std::fs::read_to_string(arch).unwrap(), "arm64");
    }

    #[test]
    fn failed_first_install_removes_published_architecture() {
        let dir = tempfile::tempdir().unwrap();
        let staged_sqsh = dir.path().join("missing.sqsh");
        let staged_arch = dir.path().join("staged.sqsh.arch");
        let sqsh = dir.path().join("image.sqsh");
        let arch = dir.path().join("image.sqsh.arch");
        std::fs::write(&staged_arch, b"arm64").unwrap();

        let error = install_staged_image(&staged_sqsh, &staged_arch, &sqsh, &arch).unwrap_err();

        assert!(error.to_string().contains("failed to install image"));
        assert!(!sqsh.exists());
        assert!(!arch.exists());
    }

    #[test]
    fn interrupted_replacement_cannot_cache_either_architecture() {
        let dir = tempfile::tempdir().unwrap();
        let staged_sqsh = dir.path().join("staged.sqsh");
        let missing_staged_arch = dir.path().join("missing.sqsh.arch");
        let sqsh = dir.path().join("image.sqsh");
        let arch = dir.path().join("image.sqsh.arch");
        std::fs::write(&staged_sqsh, b"new payload").unwrap();
        std::fs::write(&sqsh, b"old payload").unwrap();
        std::fs::write(&arch, b"amd64").unwrap();

        let error =
            install_staged_image(&staged_sqsh, &missing_staged_arch, &sqsh, &arch).unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to record image architecture"));
        assert_eq!(std::fs::read(&sqsh).unwrap(), b"new payload");
        let cached_arch = std::fs::read_to_string(&arch).ok();
        assert!(!cached_architecture_matches(
            cached_arch.as_deref(),
            "amd64"
        ));
        assert!(!cached_architecture_matches(
            cached_arch.as_deref(),
            "arm64"
        ));
    }

    #[test]
    fn different_architecture_installs_publish_one_coherent_winner() {
        let dir = tempfile::tempdir().unwrap();
        let sanitized = "docker.io+library+ubuntu+latest";
        let sqsh = dir.path().join("image.sqsh");
        let arch = dir.path().join("image.sqsh.arch");
        let first_sqsh = dir.path().join("first.sqsh");
        let first_arch = dir.path().join("first.sqsh.arch");
        let second_sqsh = dir.path().join("second.sqsh");
        let second_arch = dir.path().join("second.sqsh.arch");
        std::fs::write(&first_sqsh, b"arm64 payload").unwrap();
        std::fs::write(&first_arch, b"arm64").unwrap();
        std::fs::write(&second_sqsh, b"amd64 payload").unwrap();
        std::fs::write(&second_arch, b"amd64").unwrap();

        let first_lock = lock_image_cache(dir.path(), sanitized).unwrap();
        let contender = open_image_cache_lock(dir.path(), sanitized).unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));

        let output_dir = dir.path().to_path_buf();
        let sqsh_for_thread = sqsh.clone();
        let arch_for_thread = arch.clone();
        let second = std::thread::spawn(move || {
            let _lock = lock_image_cache(&output_dir, sanitized).unwrap();
            install_staged_image(
                &second_sqsh,
                &second_arch,
                &sqsh_for_thread,
                &arch_for_thread,
            )
            .unwrap();
        });

        install_staged_image(&first_sqsh, &first_arch, &sqsh, &arch).unwrap();
        drop(first_lock);
        second.join().unwrap();

        assert_eq!(std::fs::read(&sqsh).unwrap(), b"amd64 payload");
        assert_eq!(std::fs::read_to_string(&arch).unwrap(), "amd64");
    }

    #[test]
    fn direct_manifest_config_must_match_requested_platform() {
        validate_image_configuration(r#"{"architecture":"arm64","os":"linux"}"#, "aarch64")
            .unwrap();
    }

    #[test]
    fn direct_manifest_config_rejects_wrong_architecture() {
        let error =
            validate_image_configuration(r#"{"architecture":"amd64","os":"linux"}"#, "arm64")
                .unwrap_err();

        assert_eq!(
            error.to_string(),
            "direct manifest platform linux/amd64 does not match requested linux/arm64"
        );
    }

    #[test]
    fn direct_manifest_config_rejects_non_linux_images() {
        let error =
            validate_image_configuration(r#"{"architecture":"amd64","os":"windows"}"#, "x86_64")
                .unwrap_err();

        assert_eq!(
            error.to_string(),
            "direct manifest platform windows/amd64 does not match requested linux/amd64"
        );
    }

    #[test]
    fn test_manifest_digest_error_includes_requested_arch() {
        let error = manifest_digest_for_arch(MULTI_ARCH_MANIFEST, "riscv64").unwrap_err();
        assert_eq!(
            error.to_string(),
            "no linux/riscv64 manifest found in manifest list"
        );
    }

    #[test]
    fn test_parse_dockerhub_official() {
        let r = parse_image_ref("ubuntu:22.04");
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.tag, "22.04");
    }

    #[test]
    fn test_parse_dockerhub_user() {
        let r = parse_image_ref("nvidia/cuda:12.0-base");
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "nvidia/cuda");
        assert_eq!(r.tag, "12.0-base");
    }

    #[test]
    fn test_parse_custom_registry() {
        let r = parse_image_ref("nvcr.io/nvidia/pytorch:24.01");
        assert_eq!(r.registry, "nvcr.io");
        assert_eq!(r.repository, "nvidia/pytorch");
        assert_eq!(r.tag, "24.01");
    }

    #[test]
    fn test_parse_ghcr() {
        let r = parse_image_ref("ghcr.io/org/repo:v1.2.3");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "org/repo");
        assert_eq!(r.tag, "v1.2.3");
    }

    #[test]
    fn test_parse_no_tag() {
        let r = parse_image_ref("alpine");
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn test_parse_docker_prefix() {
        let r = parse_image_ref("docker://ubuntu:22.04");
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.tag, "22.04");
    }

    #[test]
    fn test_parse_localhost_registry() {
        let r = parse_image_ref("localhost:5000/myimage:dev");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "myimage");
        assert_eq!(r.tag, "dev");
    }

    #[test]
    fn test_registry_base_url() {
        assert_eq!(
            registry_base_url("docker.io"),
            "https://registry-1.docker.io"
        );
        assert_eq!(registry_base_url("ghcr.io"), "https://ghcr.io");
        assert_eq!(registry_base_url("localhost:5000"), "http://localhost:5000");
    }

    #[test]
    fn test_canonical_equivalent_refs_collapse() {
        // All of these reference the same Docker Hub official image and must
        // resolve to a single canonical name / filename stem.
        let expected = "docker.io/library/busybox:latest";
        for r in [
            "busybox",
            "busybox:latest",
            "docker://busybox",
            "docker://busybox:latest",
            "docker.io/library/busybox:latest",
        ] {
            assert_eq!(parse_image_ref(r).canonical(), expected, "ref: {}", r);
            assert_eq!(
                image_file_stem(r),
                "docker.io+library+busybox+latest",
                "ref: {}",
                r
            );
        }
    }

    #[test]
    fn test_canonical_custom_registry() {
        assert_eq!(
            parse_image_ref("nvcr.io/nvidia/pytorch:24.01").canonical(),
            "nvcr.io/nvidia/pytorch:24.01"
        );
        assert_eq!(
            image_file_stem("nvcr.io/nvidia/pytorch:24.01"),
            "nvcr.io+nvidia+pytorch+24.01"
        );
    }

    #[test]
    fn test_canonical_port_bearing_registry() {
        let r = parse_image_ref("localhost:5000/myimage:dev");
        assert_eq!(r.canonical(), "localhost:5000/myimage:dev");
        assert_eq!(
            image_file_stem("localhost:5000/myimage:dev"),
            "localhost+5000+myimage+dev"
        );
    }

    #[test]
    fn test_display_name() {
        assert_eq!(
            display_name("docker.io+library+busybox+latest"),
            "docker.io/library/busybox:latest"
        );
        assert_eq!(
            display_name("nvcr.io+nvidia+pytorch+24.01"),
            "nvcr.io/nvidia/pytorch:24.01"
        );
        assert_eq!(display_name("alpine"), "alpine");
    }

    #[test]
    fn test_sanitize() {
        assert_eq!(sanitize_name("ubuntu:22.04"), "ubuntu+22.04");
        assert_eq!(
            sanitize_name("docker://nvcr.io/nvidia/pytorch:24.01"),
            "nvcr.io+nvidia+pytorch+24.01"
        );
    }
}
