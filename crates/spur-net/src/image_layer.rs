// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context};
use flate2::read::MultiGzDecoder;
use tracing::debug;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayerCompression {
    Gzip,
    Uncompressed,
    Zstd,
}

fn detect_compression(media_type: Option<&str>, data: &[u8]) -> LayerCompression {
    match media_type.filter(|value| !value.is_empty()) {
        Some(value) if value.ends_with("+gzip") || value.ends_with(".gzip") => {
            LayerCompression::Gzip
        }
        Some(value) if value.ends_with("+zstd") || value.ends_with(".zstd") => {
            LayerCompression::Zstd
        }
        Some(value) if value.ends_with(".tar") => LayerCompression::Uncompressed,
        _ if data.starts_with(&[0x1f, 0x8b]) => LayerCompression::Gzip,
        _ if is_zstd(data) => LayerCompression::Zstd,
        _ => LayerCompression::Uncompressed,
    }
}

fn is_zstd(data: &[u8]) -> bool {
    data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
        || matches!(data, [0x50..=0x5f, 0x2a, 0x4d, 0x18, ..])
}

pub fn decode<'a>(data: &'a [u8], media_type: Option<&str>) -> anyhow::Result<Box<dyn Read + 'a>> {
    match detect_compression(media_type, data) {
        LayerCompression::Gzip => Ok(Box::new(MultiGzDecoder::new(data))),
        LayerCompression::Uncompressed => Ok(Box::new(data)),
        LayerCompression::Zstd => Ok(Box::new(zstd::stream::read::Decoder::new(data)?)),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryUnpackPolicy {
    Strict,
    SkipFailedEntries,
}

#[derive(Debug, Eq, PartialEq)]
enum Whiteout {
    Remove(PathBuf),
    Opaque(PathBuf),
}

pub fn extract(
    data: &[u8],
    media_type: Option<&str>,
    dest: &Path,
    policy: EntryUnpackPolicy,
) -> anyhow::Result<()> {
    let mut whiteouts = collect_whiteouts(data, media_type)?;
    whiteouts.sort_by_key(Whiteout::application_order);
    fs::create_dir_all(dest)
        .with_context(|| format!("failed to create layer root {}", dest.display()))?;
    let root = dest
        .canonicalize()
        .with_context(|| format!("failed to resolve layer root {}", dest.display()))?;

    for whiteout in &whiteouts {
        apply_whiteout(&root, whiteout)?;
    }

    unpack_entries(data, media_type, &root, policy)
}

fn collect_whiteouts(data: &[u8], media_type: Option<&str>) -> anyhow::Result<Vec<Whiteout>> {
    let mut archive = tar::Archive::new(decode(data, media_type)?);
    let mut whiteouts = Vec::new();

    {
        let entries = archive.entries().context("failed to read layer entries")?;
        for entry in entries {
            let mut entry = entry.context("failed to read layer entry")?;
            let raw_path = entry.path_bytes().into_owned();
            let entry_type = entry.header().entry_type();
            if is_metadata_entry(entry_type) {
                std::io::copy(&mut entry, &mut std::io::sink())
                    .context("failed to read layer metadata entry")?;
                continue;
            }
            let path = safe_relative_path(&entry.path().context("invalid layer entry path")?)?;
            let whiteout = classify_whiteout(&path, &raw_path)?;
            if whiteout.is_some()
                && (!entry.header().entry_type().is_file() || entry.header().size()? != 0)
            {
                bail!("whiteout is not an empty regular file: {}", path.display());
            }
            std::io::copy(&mut entry, &mut std::io::sink())
                .with_context(|| format!("failed to read layer entry {}", path.display()))?;
            if let Some(whiteout) = whiteout {
                whiteouts.push(whiteout);
            }
        }
    }
    std::io::copy(&mut archive.into_inner(), &mut std::io::sink())
        .context("failed to finish reading layer")?;

    Ok(whiteouts)
}

fn is_metadata_entry(entry_type: tar::EntryType) -> bool {
    entry_type.is_pax_global_extensions()
        || entry_type.is_pax_local_extensions()
        || entry_type.is_gnu_longname()
        || entry_type.is_gnu_longlink()
}

fn classify_whiteout(path: &Path, raw_path: &[u8]) -> anyhow::Result<Option<Whiteout>> {
    let raw_filename = raw_path
        .rsplit(|byte| *byte == b'/')
        .find(|component| !component.is_empty())
        .unwrap_or_default();
    if !raw_filename.starts_with(b".wh.") {
        return Ok(None);
    }
    let filename = std::str::from_utf8(raw_filename)
        .with_context(|| format!("whiteout path is not valid UTF-8: {}", path.display()))?;
    let parent = path.parent().unwrap_or_else(|| Path::new(""));

    if filename == ".wh..wh..opq" {
        return Ok(Some(Whiteout::Opaque(parent.to_path_buf())));
    }

    let Some(target) = filename.strip_prefix(".wh.") else {
        return Ok(None);
    };
    let mut components = Path::new(target).components();
    let Some(Component::Normal(target)) = components.next() else {
        bail!("invalid whiteout path: {}", path.display());
    };
    if components.next().is_some() {
        bail!("invalid whiteout path: {}", path.display());
    }

    Ok(Some(Whiteout::Remove(parent.join(target))))
}

impl Whiteout {
    fn application_order(&self) -> (usize, u8) {
        match self {
            Self::Remove(path) => (path.components().count(), 0),
            Self::Opaque(path) => (path.components().count(), 1),
        }
    }
}

fn safe_relative_path(path: &Path) -> anyhow::Result<PathBuf> {
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("layer entry path escapes rootfs: {}", path.display());
            }
        }
    }
    Ok(safe)
}

fn apply_whiteout(root: &Path, whiteout: &Whiteout) -> anyhow::Result<()> {
    match whiteout {
        Whiteout::Remove(target) => {
            let parent = target.parent().unwrap_or_else(|| Path::new(""));
            let parent = ensure_directory(root, parent)?;
            let name = target
                .file_name()
                .context("whiteout target has no filename")?;
            remove_path(&parent.join(name))
        }
        Whiteout::Opaque(directory) => {
            let directory = ensure_directory(root, directory)?;
            for child in fs::read_dir(&directory).with_context(|| {
                format!("failed to read opaque directory {}", directory.display())
            })? {
                remove_path(&child?.path())?;
            }
            Ok(())
        }
    }
}

fn ensure_directory(root: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!("invalid layer directory: {}", relative.display());
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !metadata.is_dir() => {
                remove_path(&current)?;
                fs::create_dir(&current).with_context(|| {
                    format!("failed to create layer directory {}", current.display())
                })?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!("failed to create layer directory {}", current.display())
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect layer directory {}", current.display())
                });
            }
        }
    }
    Ok(current)
}

fn reconcile_entry_target(
    root: &Path,
    path: &Path,
    incoming_is_directory: bool,
) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    let parent = ensure_directory(root, path.parent().unwrap_or_else(|| Path::new("")))?;
    let target = parent.join(path.file_name().context("layer entry has no filename")?);
    match fs::symlink_metadata(&target) {
        Ok(metadata) => {
            let existing_is_dir = metadata.is_dir();
            let existing_is_symlink = metadata.file_type().is_symlink();
            if incoming_is_directory {
                if existing_is_dir {
                    Ok(())
                } else {
                    remove_path(&target)
                }
            } else if existing_is_dir || existing_is_symlink {
                remove_path(&target)
            } else {
                Ok(())
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect layer target {}", target.display())),
    }
}

fn replace_with_hardlink(root: &Path, path: &Path, link_name: &Path) -> anyhow::Result<()> {
    let link_name = safe_relative_path(link_name)?;
    if link_name.as_os_str().is_empty() {
        bail!("hardlink source is empty");
    }
    let source = root.join(&link_name);
    let resolved_source = source
        .canonicalize()
        .with_context(|| format!("failed to resolve hardlink source {}", link_name.display()))?;
    if !resolved_source.starts_with(root) {
        bail!("hardlink source escapes rootfs: {}", link_name.display());
    }

    let parent = ensure_directory(root, path.parent().unwrap_or_else(|| Path::new("")))?;
    let target = parent.join(path.file_name().context("layer entry has no filename")?);
    let staging = tempfile::Builder::new()
        .prefix(".spur-layer-link-")
        .tempdir_in(&parent)
        .with_context(|| format!("failed to stage hardlink for {}", path.display()))?;
    let staged_link = staging.path().join("link");
    fs::hard_link(&source, &staged_link).with_context(|| {
        format!(
            "failed to stage hardlink {} to {}",
            link_name.display(),
            path.display()
        )
    })?;

    match fs::symlink_metadata(&target) {
        Ok(_) => {
            let backup = staging.path().join("backup");
            fs::rename(&target, &backup)
                .with_context(|| format!("failed to back up layer target {}", path.display()))?;
            if let Err(error) = fs::rename(&staged_link, &target) {
                if let Err(restore_error) = fs::rename(&backup, &target) {
                    let recovery_directory = staging.keep();
                    bail!(
                        "failed to replace layer target {}: {error}; failed to restore it: \
                         {restore_error}; recovery files retained at {}",
                        path.display(),
                        recovery_directory.display()
                    );
                }
                return Err(error)
                    .with_context(|| format!("failed to replace layer target {}", path.display()));
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::rename(&staged_link, &target)
                .with_context(|| format!("failed to install hardlink {}", path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect layer target {}", path.display()));
        }
    }

    Ok(())
}

fn remove_path(path: &Path) -> anyhow::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect layer path {}", path.display()));
        }
    };

    if metadata.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove layer directory {}", path.display()))
    } else {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove layer path {}", path.display()))
    }
}

fn unpack_entries(
    data: &[u8],
    media_type: Option<&str>,
    dest: &Path,
    policy: EntryUnpackPolicy,
) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(decode(data, media_type)?);
    archive.set_overwrite(true);

    for entry in archive.entries().context("failed to read layer entries")? {
        let mut entry = entry.context("failed to read layer entry")?;
        let entry_type = entry.header().entry_type();
        if is_metadata_entry(entry_type) {
            continue;
        }
        let raw_path = entry.path_bytes().into_owned();
        let path = safe_relative_path(&entry.path().context("invalid layer entry path")?)?;
        if classify_whiteout(&path, &raw_path)?.is_some() {
            continue;
        }
        if entry_type.is_hard_link() {
            let result = (|| -> anyhow::Result<()> {
                let link_name = entry
                    .link_name()
                    .context("failed to read hardlink source")?
                    .context("hardlink source is missing")?;
                replace_with_hardlink(dest, &path, &link_name)
            })();
            let error = match result {
                Ok(()) => continue,
                Err(error) => error,
            };
            match policy {
                EntryUnpackPolicy::Strict => {
                    return Err(error).with_context(|| {
                        format!("failed to unpack layer entry {}", path.display())
                    });
                }
                EntryUnpackPolicy::SkipFailedEntries => {
                    debug!(path = %path.display(), %error, "skipping layer entry");
                    continue;
                }
            }
        }
        let incoming_is_directory = entry_type.is_dir()
            || (entry.header().as_ustar().is_none() && raw_path.ends_with(b"/"));
        reconcile_entry_target(dest, &path, incoming_is_directory)?;

        let error = match entry.unpack_in(dest) {
            Ok(true) => continue,
            Ok(false) => anyhow::anyhow!("entry was rejected as unsafe"),
            Err(error) => error.into(),
        };
        match policy {
            EntryUnpackPolicy::Strict => {
                return Err(error)
                    .with_context(|| format!("failed to unpack layer entry {}", path.display()));
            }
            EntryUnpackPolicy::SkipFailedEntries => {
                debug!(path = %path.display(), %error, "skipping layer entry");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use flate2::{write::GzEncoder, Compression};

    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn read_layer(data: &[u8], media_type: Option<&str>) -> anyhow::Result<Vec<u8>> {
        let mut decoded = Vec::new();
        decode(data, media_type)?.read_to_end(&mut decoded)?;
        Ok(decoded)
    }

    fn tar_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, path, *contents).unwrap();
        }
        archive.finish().unwrap();
        drop(archive);
        data
    }

    fn tar_file_with_literal_path(path: &[u8]) -> Vec<u8> {
        assert!(path.len() < 100);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.as_mut_bytes()[..path.len()].copy_from_slice(path);
        header.set_cksum();

        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);
        archive.append(&header, std::io::empty()).unwrap();
        archive.finish().unwrap();
        drop(archive);
        data
    }

    fn tar_with_pax_global_header(path: &str) -> Vec<u8> {
        let pax_record = b"18 comment=global\n";
        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);

        let mut pax_header = tar::Header::new_ustar();
        pax_header.set_entry_type(tar::EntryType::XGlobalHeader);
        pax_header.set_size(pax_record.len() as u64);
        pax_header.set_mode(0o644);
        pax_header.set_cksum();
        archive
            .append_data(&mut pax_header, path, pax_record.as_slice())
            .unwrap();

        let mut file_header = tar::Header::new_gnu();
        file_header.set_size(7);
        file_header.set_mode(0o644);
        file_header.set_cksum();
        archive
            .append_data(&mut file_header, "current.txt", b"current".as_slice())
            .unwrap();

        archive.finish().unwrap();
        drop(archive);
        data
    }

    fn tar_with_legacy_directory(path: &str) -> Vec<u8> {
        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);
        let mut header = tar::Header::new_old();
        header.set_path(path).unwrap();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        archive.append(&header, std::io::empty()).unwrap();
        archive.finish().unwrap();
        drop(archive);
        data
    }

    fn tar_with_file_and_hardlink(source: &str, target: &str) -> Vec<u8> {
        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);

        let mut file_header = tar::Header::new_gnu();
        file_header.set_size(7);
        file_header.set_mode(0o644);
        file_header.set_cksum();
        archive
            .append_data(&mut file_header, source, b"current".as_slice())
            .unwrap();

        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::hard_link());
        link_header.set_size(0);
        link_header.set_mode(0o644);
        archive
            .append_link(&mut link_header, target, source)
            .unwrap();

        archive.finish().unwrap();
        drop(archive);
        data
    }

    fn tar_hardlink(source: &str, target: &str) -> Vec<u8> {
        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);
        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::hard_link());
        link_header.set_size(0);
        link_header.set_mode(0o644);
        archive
            .append_link(&mut link_header, target, source)
            .unwrap();
        archive.finish().unwrap();
        drop(archive);
        data
    }

    fn assert_failed_entry_result(result: anyhow::Result<()>, policy: EntryUnpackPolicy) {
        match policy {
            EntryUnpackPolicy::Strict => assert!(result.is_err()),
            EntryUnpackPolicy::SkipFailedEntries => result.unwrap(),
        }
    }

    #[test]
    fn media_type_suffix_selects_compression_before_magic() {
        assert_eq!(
            detect_compression(Some("application/example+gzip"), b"plain"),
            LayerCompression::Gzip
        );
        assert_eq!(
            detect_compression(Some("application/example.tar.gzip"), b"plain"),
            LayerCompression::Gzip
        );
        assert_eq!(
            detect_compression(Some("application/example+zstd"), b"plain"),
            LayerCompression::Zstd
        );
        assert_eq!(
            detect_compression(Some("application/example.tar.zstd"), b"plain"),
            LayerCompression::Zstd
        );
        assert_eq!(
            detect_compression(Some("application/example.tar"), &[0x1f, 0x8b]),
            LayerCompression::Uncompressed
        );
    }

    #[test]
    fn decode_supports_oci_and_docker_media_types() {
        let payload = b"layer payload";
        let gzip = gzip(payload);
        let zstd = zstd::stream::encode_all(payload.as_slice(), 0).unwrap();

        for media_type in [
            "application/vnd.oci.image.layer.v1.tar+gzip",
            "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
            "application/vnd.docker.image.rootfs.diff.tar.gzip",
            "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
        ] {
            assert_eq!(read_layer(&gzip, Some(media_type)).unwrap(), payload);
        }

        for media_type in [
            "application/vnd.oci.image.layer.v1.tar+zstd",
            "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd",
        ] {
            assert_eq!(read_layer(&zstd, Some(media_type)).unwrap(), payload);
        }

        for media_type in [
            "application/vnd.oci.image.layer.v1.tar",
            "application/vnd.oci.image.layer.nondistributable.v1.tar",
            "application/vnd.docker.image.rootfs.diff.tar",
        ] {
            assert_eq!(read_layer(payload, Some(media_type)).unwrap(), payload);
        }
    }

    #[test]
    fn decode_uses_magic_for_missing_or_unknown_media_type() {
        let payload = b"layer payload";
        let gzip = gzip(payload);
        let zstd = zstd::stream::encode_all(payload.as_slice(), 0).unwrap();

        for media_type in [None, Some(""), Some("application/octet-stream")] {
            assert_eq!(read_layer(payload, media_type).unwrap(), payload);
            assert_eq!(read_layer(&gzip, media_type).unwrap(), payload);
            assert_eq!(read_layer(&zstd, media_type).unwrap(), payload);
        }
    }

    #[test]
    fn decode_uses_declared_compression_before_magic() {
        let gzip = gzip(b"layer payload");
        let zstd = zstd::stream::encode_all(b"layer payload".as_slice(), 0).unwrap();

        assert!(read_layer(&zstd, Some("application/vnd.oci.image.layer.v1.tar+gzip")).is_err());
        assert!(read_layer(&gzip, Some("application/vnd.oci.image.layer.v1.tar+zstd")).is_err());
    }

    #[test]
    fn decode_supports_concatenated_compression_frames() {
        let mut gzip_frames = gzip(b"first ");
        gzip_frames.extend(gzip(b"second"));
        assert_eq!(read_layer(&gzip_frames, None).unwrap(), b"first second");

        let mut zstd_frames = zstd::stream::encode_all(b"first ".as_slice(), 0).unwrap();
        zstd_frames.extend(zstd::stream::encode_all(b"second".as_slice(), 0).unwrap());
        assert_eq!(read_layer(&zstd_frames, None).unwrap(), b"first second");
    }

    #[test]
    fn decode_rejects_truncated_compression_frames() {
        let mut gzip = gzip(b"layer payload");
        gzip.truncate(gzip.len() - 1);
        assert!(read_layer(&gzip, None).is_err());

        let mut zstd = zstd::stream::encode_all(b"layer payload".as_slice(), 0).unwrap();
        zstd.truncate(zstd.len() - 1);
        assert!(read_layer(&zstd, None).is_err());
    }

    #[test]
    fn decode_detects_zstd_skippable_magic() {
        let mut layer = vec![0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0];
        layer.extend(zstd::stream::encode_all(b"zstd payload".as_slice(), 0).unwrap());

        assert_eq!(
            read_layer(&layer, Some("application/octet-stream")).unwrap(),
            b"zstd payload"
        );
    }

    #[test]
    fn regular_whiteout_removes_lower_directory_and_preserves_same_layer_entry() {
        let rootfs = tempfile::tempdir().unwrap();
        let removed = rootfs.path().join("nested/replaced/lower.txt");
        let retained = rootfs.path().join("nested/retained.txt");
        fs::create_dir_all(removed.parent().unwrap()).unwrap();
        fs::write(&removed, b"lower").unwrap();
        fs::write(&retained, b"retained").unwrap();
        let layer = tar_files(&[
            ("nested/replaced/current.txt", b"current"),
            ("nested/.wh.replaced", b""),
        ]);

        extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap();

        assert!(!removed.exists());
        assert_eq!(
            fs::read(rootfs.path().join("nested/replaced/current.txt")).unwrap(),
            b"current"
        );
        assert_eq!(fs::read(retained).unwrap(), b"retained");
        assert!(!rootfs.path().join("nested/.wh.replaced").exists());
    }

    #[test]
    fn opaque_whiteout_preserves_same_layer_entries_on_both_sides_of_marker() {
        let rootfs = tempfile::tempdir().unwrap();
        let directory = rootfs.path().join("nested");
        fs::create_dir_all(directory.join("lower-dir")).unwrap();
        fs::write(directory.join("lower.txt"), b"lower").unwrap();
        fs::write(directory.join("lower-dir/child.txt"), b"lower").unwrap();
        let layer = tar_files(&[
            ("nested/before.txt", b"before"),
            ("nested/.wh..wh..opq", b""),
            ("nested/after.txt", b"after"),
        ]);

        extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap();

        assert!(!directory.join("lower.txt").exists());
        assert!(!directory.join("lower-dir").exists());
        assert_eq!(fs::read(directory.join("before.txt")).unwrap(), b"before");
        assert_eq!(fs::read(directory.join("after.txt")).unwrap(), b"after");
        assert!(!directory.join(".wh..wh..opq").exists());
    }

    #[test]
    fn whiteouts_replace_directories_and_non_directory_ancestors() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(rootfs.path().join("old-directory")).unwrap();
        fs::write(rootfs.path().join("old-directory/lower.txt"), b"lower").unwrap();
        fs::write(rootfs.path().join("old-file"), b"lower").unwrap();
        let layer = tar_files(&[
            ("old-directory", b"current file"),
            (".wh.old-directory", b""),
            ("old-file/current.txt", b"current directory"),
            (".wh.old-file", b""),
        ]);

        extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap();

        assert_eq!(
            fs::read(rootfs.path().join("old-directory")).unwrap(),
            b"current file"
        );
        assert_eq!(
            fs::read(rootfs.path().join("old-file/current.txt")).unwrap(),
            b"current directory"
        );
    }

    #[test]
    fn incoming_entries_reconcile_directory_and_file_types_without_whiteouts() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(rootfs.path().join("lower-directory")).unwrap();
        fs::write(rootfs.path().join("lower-directory/child.txt"), b"lower").unwrap();
        fs::write(rootfs.path().join("lower-file"), b"lower").unwrap();
        let layer = tar_files(&[
            ("lower-directory", b"current file"),
            ("lower-file/current.txt", b"current directory"),
        ]);

        extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap();

        assert_eq!(
            fs::read(rootfs.path().join("lower-directory")).unwrap(),
            b"current file"
        );
        assert_eq!(
            fs::read(rootfs.path().join("lower-file/current.txt")).unwrap(),
            b"current directory"
        );
    }

    #[test]
    fn hardlink_replaces_lower_file_for_all_unpack_policies() {
        let layer = tar_with_file_and_hardlink("source", "replaced");

        for policy in [
            EntryUnpackPolicy::Strict,
            EntryUnpackPolicy::SkipFailedEntries,
        ] {
            let rootfs = tempfile::tempdir().unwrap();
            fs::write(rootfs.path().join("replaced"), b"lower").unwrap();

            extract(&layer, None, rootfs.path(), policy).unwrap();

            assert_eq!(
                fs::read(rootfs.path().join("replaced")).unwrap(),
                b"current"
            );
            fs::write(rootfs.path().join("source"), b"updated").unwrap();
            assert_eq!(
                fs::read(rootfs.path().join("replaced")).unwrap(),
                b"updated"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_preserves_in_root_symlink_source_semantics() {
        use std::os::unix::fs::symlink;

        let layer = tar_hardlink("source", "replaced");

        for policy in [
            EntryUnpackPolicy::Strict,
            EntryUnpackPolicy::SkipFailedEntries,
        ] {
            let rootfs = tempfile::tempdir().unwrap();
            fs::write(rootfs.path().join("source-target"), b"current").unwrap();
            symlink("source-target", rootfs.path().join("source")).unwrap();
            fs::write(rootfs.path().join("replaced"), b"lower").unwrap();

            extract(&layer, None, rootfs.path(), policy).unwrap();

            assert!(fs::symlink_metadata(rootfs.path().join("replaced"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                fs::read_link(rootfs.path().join("replaced")).unwrap(),
                Path::new("source-target")
            );
        }
    }

    #[test]
    fn missing_hardlink_source_preserves_lower_target_for_all_unpack_policies() {
        let layer = tar_hardlink("missing", "replaced");

        for policy in [
            EntryUnpackPolicy::Strict,
            EntryUnpackPolicy::SkipFailedEntries,
        ] {
            let rootfs = tempfile::tempdir().unwrap();
            let replaced = rootfs.path().join("replaced");
            fs::write(&replaced, b"lower").unwrap();

            assert_failed_entry_result(extract(&layer, None, rootfs.path(), policy), policy);

            assert_eq!(fs::read(replaced).unwrap(), b"lower");
        }
    }

    #[test]
    fn unusable_hardlink_source_preserves_lower_target_for_all_unpack_policies() {
        let layer = tar_hardlink("source", "replaced");

        for policy in [
            EntryUnpackPolicy::Strict,
            EntryUnpackPolicy::SkipFailedEntries,
        ] {
            let rootfs = tempfile::tempdir().unwrap();
            let replaced = rootfs.path().join("replaced");
            fs::create_dir(rootfs.path().join("source")).unwrap();
            fs::write(&replaced, b"lower").unwrap();

            assert_failed_entry_result(extract(&layer, None, rootfs.path(), policy), policy);

            assert_eq!(fs::read(replaced).unwrap(), b"lower");
        }
    }

    #[test]
    fn parent_relative_hardlink_source_preserves_lower_target_for_all_unpack_policies() {
        let layer = tar_hardlink("../outside", "replaced");

        for policy in [
            EntryUnpackPolicy::Strict,
            EntryUnpackPolicy::SkipFailedEntries,
        ] {
            let rootfs = tempfile::tempdir().unwrap();
            let replaced = rootfs.path().join("replaced");
            fs::write(&replaced, b"lower").unwrap();

            assert_failed_entry_result(extract(&layer, None, rootfs.path(), policy), policy);

            assert_eq!(fs::read(replaced).unwrap(), b"lower");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_hardlink_source_preserves_lower_target_for_all_unpack_policies() {
        use std::os::unix::fs::symlink;

        let layer = tar_hardlink("escape", "replaced");

        for policy in [
            EntryUnpackPolicy::Strict,
            EntryUnpackPolicy::SkipFailedEntries,
        ] {
            let base = tempfile::tempdir().unwrap();
            let rootfs = base.path().join("rootfs");
            let outside = base.path().join("outside");
            fs::create_dir(&rootfs).unwrap();
            fs::write(&outside, b"outside").unwrap();
            symlink(&outside, rootfs.join("escape")).unwrap();
            let replaced = rootfs.join("replaced");
            fs::write(&replaced, b"lower").unwrap();

            assert_failed_entry_result(extract(&layer, None, &rootfs, policy), policy);

            assert_eq!(fs::read(replaced).unwrap(), b"lower");
            assert_eq!(fs::read(&outside).unwrap(), b"outside");
        }
    }

    #[test]
    fn pax_global_header_does_not_replace_colliding_lower_path() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::write(rootfs.path().join("collision"), b"lower").unwrap();
        let layer = tar_with_pax_global_header("collision");

        extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap();

        assert_eq!(fs::read(rootfs.path().join("collision")).unwrap(), b"lower");
        assert_eq!(
            fs::read(rootfs.path().join("current.txt")).unwrap(),
            b"current"
        );
    }

    #[test]
    fn legacy_trailing_slash_directory_preserves_lower_children() {
        let rootfs = tempfile::tempdir().unwrap();
        let lower = rootfs.path().join("legacy/lower.txt");
        fs::create_dir_all(lower.parent().unwrap()).unwrap();
        fs::write(&lower, b"lower").unwrap();
        let layer = tar_with_legacy_directory("legacy/");

        extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap();

        assert_eq!(fs::read(lower).unwrap(), b"lower");
    }

    #[cfg(unix)]
    #[test]
    fn incoming_directory_replaces_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let base = tempfile::tempdir().unwrap();
        let rootfs = base.path().join("rootfs");
        let outside = base.path().join("outside");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("victim"), b"safe").unwrap();
        symlink(&outside, rootfs.join("replaced")).unwrap();
        let layer = tar_files(&[("replaced/current.txt", b"current")]);

        extract(&layer, None, &rootfs, EntryUnpackPolicy::Strict).unwrap();

        assert_eq!(fs::read(outside.join("victim")).unwrap(), b"safe");
        assert_eq!(
            fs::read(rootfs.join("replaced/current.txt")).unwrap(),
            b"current"
        );
        assert!(!rootfs
            .join("replaced")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn opaque_whiteout_replaces_lower_file_with_directory() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::write(rootfs.path().join("replaced"), b"lower").unwrap();
        let layer = tar_files(&[
            ("replaced/.wh..wh..opq", b""),
            ("replaced/current.txt", b"current"),
        ]);

        extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap();

        assert_eq!(
            fs::read(rootfs.path().join("replaced/current.txt")).unwrap(),
            b"current"
        );
        assert!(!rootfs.path().join("replaced/.wh..wh..opq").exists());
    }

    #[test]
    fn ancestor_whiteout_runs_before_nested_opaque_whiteout() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::write(rootfs.path().join("replaced"), b"lower file").unwrap();
        let layer = tar_files(&[
            ("replaced/.wh..wh..opq", b""),
            (".wh.replaced", b""),
            ("replaced/current.txt", b"current"),
        ]);

        extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap();

        assert_eq!(
            fs::read(rootfs.path().join("replaced/current.txt")).unwrap(),
            b"current"
        );
        assert!(!rootfs.path().join("replaced/.wh..wh..opq").exists());
    }

    #[test]
    fn degenerate_whiteout_targets_are_rejected() {
        for marker in [".wh.", ".wh..", ".wh..."] {
            let rootfs = tempfile::tempdir().unwrap();
            let layer = tar_files(&[(marker, b"")]);

            let error =
                extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap_err();

            assert!(error.to_string().contains("invalid whiteout path"));
            assert!(!rootfs.path().join(marker).exists());
        }
    }

    #[test]
    fn invalid_later_marker_prevents_earlier_whiteout_deletion() {
        let rootfs = tempfile::tempdir().unwrap();
        let victim = rootfs.path().join("victim");
        fs::write(&victim, b"safe").unwrap();
        let layer = tar_files(&[(".wh.victim", b""), (".wh.", b"")]);

        let error = extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap_err();

        assert!(error.to_string().contains("invalid whiteout path"));
        assert_eq!(fs::read(victim).unwrap(), b"safe");
    }

    #[test]
    fn nonempty_whiteout_is_rejected_before_deletion() {
        let rootfs = tempfile::tempdir().unwrap();
        let victim = rootfs.path().join("victim");
        fs::write(&victim, b"safe").unwrap();
        let layer = tar_files(&[(".wh.victim", b"not empty")]);

        let error = extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap_err();

        assert!(error.to_string().contains("not an empty regular file"));
        assert_eq!(fs::read(victim).unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_marker_shaped_path_is_rejected() {
        let rootfs = tempfile::tempdir().unwrap();
        let layer = tar_file_with_literal_path(b".wh.\xff");

        let error = extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap_err();

        assert!(error.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn marker_with_repeated_trailing_slashes_is_applied() {
        let rootfs = tempfile::tempdir().unwrap();
        let victim = rootfs.path().join("victim");
        fs::write(&victim, b"remove").unwrap();
        let layer = tar_file_with_literal_path(b".wh.victim//");

        extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap();

        assert!(!victim.exists());
        assert!(!rootfs.path().join(".wh.victim").exists());
    }

    #[test]
    fn late_compression_error_prevents_whiteout_deletion() {
        let rootfs = tempfile::tempdir().unwrap();
        let victim = rootfs.path().join("victim");
        fs::write(&victim, b"safe").unwrap();
        let mut layer = gzip(&tar_files(&[(".wh.victim", b"")]));
        layer.truncate(layer.len() - 4);

        let error = extract(&layer, None, rootfs.path(), EntryUnpackPolicy::Strict).unwrap_err();

        assert!(error.to_string().contains("failed to finish reading layer"));
        assert_eq!(fs::read(victim).unwrap(), b"safe");
    }

    #[test]
    fn parent_path_whiteout_cannot_remove_file_outside_rootfs() {
        let base = tempfile::tempdir().unwrap();
        let rootfs = base.path().join("rootfs");
        let victim = base.path().join("victim");
        fs::write(&victim, b"safe").unwrap();
        let layer = tar_file_with_literal_path(b"../.wh.victim");

        let error = extract(&layer, None, &rootfs, EntryUnpackPolicy::Strict).unwrap_err();

        assert!(error.to_string().contains("escapes rootfs"));
        assert_eq!(fs::read(victim).unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[test]
    fn absolute_path_whiteout_cannot_remove_host_file() {
        let base = tempfile::tempdir().unwrap();
        let rootfs = base.path().join("rootfs");
        let victim = base.path().join("victim");
        fs::write(&victim, b"safe").unwrap();
        let marker = base.path().join(".wh.victim");
        let layer = tar_file_with_literal_path(marker.to_str().unwrap().as_bytes());

        let error = extract(&layer, None, &rootfs, EntryUnpackPolicy::Strict).unwrap_err();

        assert!(error.to_string().contains("escapes rootfs"));
        assert_eq!(fs::read(victim).unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[test]
    fn whiteout_replaces_symlink_parent_without_touching_host_file() {
        use std::os::unix::fs::symlink;

        let base = tempfile::tempdir().unwrap();
        let rootfs = base.path().join("rootfs");
        let outside = base.path().join("outside");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("victim"), b"safe").unwrap();
        symlink(&outside, rootfs.join("escape")).unwrap();
        let layer = tar_files(&[
            ("escape/.wh..wh..opq", b""),
            ("escape/current.txt", b"current"),
        ]);

        extract(&layer, None, &rootfs, EntryUnpackPolicy::Strict).unwrap();

        assert_eq!(fs::read(outside.join("victim")).unwrap(), b"safe");
        assert_eq!(
            fs::read(rootfs.join("escape/current.txt")).unwrap(),
            b"current"
        );
    }
}
