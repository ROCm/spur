// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;

use flate2::read::MultiGzDecoder;

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
}
