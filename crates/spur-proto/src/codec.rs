// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A gRPC codec with identical wire behaviour to the default prost codec, but a
//! richer error when a decode fails. On failure it logs prost's own error plus a
//! best-effort, schema-less locator: the field path and byte offset of the first
//! length-delimited leaf that is not valid UTF-8, with a hex window and digest.
//! That offset is only a heuristic candidate — a non-UTF-8 byte can sit in a
//! legitimate bytes or packed field rather than at the real fault — so it never
//! replaces prost's error. The raw bytes may belong to another caller, so they
//! stay in the log and never go to the client; the status carries prost's
//! structural error, the length, and the candidate offset — never the bytes.

use std::marker::PhantomData;

use prost::bytes::Buf;
use prost::Message;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::Status;

/// Reuses prost's encoding (byte-identical to the default codec) and pairs it
/// with [`DiagnosticDecoder`].
pub struct DiagnosticCodec<T, U>(PhantomData<(T, U)>);

impl<T, U> Default for DiagnosticCodec<T, U> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T, U> Codec for DiagnosticCodec<T, U>
where
    T: Message + Send + 'static,
    U: Message + Default + Send + 'static,
{
    type Encode = T;
    type Decode = U;
    type Encoder = DiagnosticEncoder<T>;
    type Decoder = DiagnosticDecoder<U>;

    fn encoder(&mut self) -> Self::Encoder {
        DiagnosticEncoder(PhantomData)
    }

    fn decoder(&mut self) -> Self::Decoder {
        DiagnosticDecoder(PhantomData)
    }
}

pub struct DiagnosticEncoder<T>(PhantomData<T>);

impl<T: Message> Encoder for DiagnosticEncoder<T> {
    type Item = T;
    type Error = Status;

    fn encode(&mut self, item: T, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        item.encode(dst)
            .map_err(|e| Status::internal(format!("failed to encode message: {e}")))
    }
}

pub struct DiagnosticDecoder<U>(PhantomData<U>);

impl<U: Message + Default> Decoder for DiagnosticDecoder<U> {
    type Item = U;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<U>, Status> {
        // Zero-copy: hand the whole (de-framed) message to prost without a copy.
        let bytes = src.copy_to_bytes(src.remaining());
        match U::decode(bytes.clone()) {
            Ok(item) => Ok(Some(item)),
            Err(error) => Err(describe_decode_failure::<U>(&bytes, &error)),
        }
    }
}

/// Stable fragment every request-decode-failure status from this codec carries
/// (`"<type> failed to decode (<n> bytes)…"`). A server matches it to recognise
/// its own inbound request-decode failures and tell them apart from an unrelated
/// `"decode …"` error, such as a forwarded response that will not decode.
pub const DECODE_FAILURE_MARKER: &str = "failed to decode (";

/// Logs the message shape and prost's error, and returns a status carrying that
/// error. The offset is a schema-less heuristic: a non-UTF-8 byte may be a valid
/// bytes/packed field rather than the fault, so it is only ever a candidate.
fn describe_decode_failure<U>(bytes: &[u8], error: &prost::DecodeError) -> Status {
    let type_name = short_type_name::<U>();
    let len = bytes.len();
    let digest = sha256_hex(bytes);
    match first_invalid_utf8(bytes) {
        Some((path, offset)) => {
            tracing::warn!(
                message_type = type_name,
                length = len,
                sha256 = %digest,
                error = %error,
                candidate_field_path = %path_str(&path),
                candidate_offset = offset,
                window = %hex_window(bytes, offset),
                "a message failed to decode; a non-UTF-8 byte in a length-delimited \
                 field is a heuristic candidate for the fault"
            );
            Status::internal(format!(
                "{type_name} failed to decode ({len} bytes): {error}; \
                 candidate non-UTF-8 byte at offset {offset}"
            ))
        }
        None => {
            tracing::warn!(
                message_type = type_name,
                length = len,
                sha256 = %digest,
                error = %error,
                "a message failed to decode"
            );
            Status::internal(format!(
                "{type_name} failed to decode ({len} bytes): {error}"
            ))
        }
    }
}

/// The last `::`-separated segment of a type name, e.g. `SubmitJobRequest`.
fn short_type_name<U>() -> &'static str {
    std::any::type_name::<U>()
        .rsplit("::")
        .next()
        .unwrap_or("message")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    to_hex(&Sha256::digest(bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn path_str(path: &[u32]) -> String {
    path.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// A hex rendering of up to 32 bytes starting at `offset`, for the daemon log.
fn hex_window(bytes: &[u8], offset: usize) -> String {
    const WINDOW: usize = 32;
    let end = bytes.len().min(offset + WINDOW);
    to_hex(&bytes[offset.min(bytes.len())..end])
}

/// The protobuf maximum of 10 bytes for a 64-bit varint.
const MAX_VARINT_BYTES: usize = 10;

/// Reads a base-128 varint, returning its value and the bytes consumed.
fn read_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    for (i, &byte) in bytes.iter().take(MAX_VARINT_BYTES).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Recursion cap for the diagnostic walker: a body reaches this path only by
/// failing to decode, so it may nest length-delimited fields arbitrarily deep.
/// Capping (near prost's own recursion limit) stops that overflowing the stack.
const MAX_DIAGNOSTIC_DEPTH: usize = 100;

/// Walk the protobuf and return the field path and absolute offset of the first
/// length-delimited leaf whose bytes are not valid UTF-8. A length-delimited
/// field that parses cleanly as a nested message is recursed into rather than
/// treated as a leaf, so corruption is attributed to the innermost string.
fn first_invalid_utf8(bytes: &[u8]) -> Option<(Vec<u32>, usize)> {
    let mut best: Option<(Vec<u32>, usize)> = None;
    let mut path: Vec<u32> = Vec::new();
    walk_fields(bytes, 0, &mut path, &mut best);
    best
}

fn walk_fields(
    bytes: &[u8],
    base: usize,
    path: &mut Vec<u32>,
    best: &mut Option<(Vec<u32>, usize)>,
) {
    if path.len() >= MAX_DIAGNOSTIC_DEPTH {
        return;
    }
    let mut pos = 0;
    while pos < bytes.len() {
        let Some((tag, n)) = read_varint(&bytes[pos..]) else {
            return;
        };
        pos += n;
        let field = (tag >> 3) as u32;
        match tag & 0x7 {
            // varint
            0 => match read_varint(&bytes[pos..]) {
                Some((_, m)) => pos += m,
                None => return,
            },
            // 64-bit
            1 => pos += 8,
            // length-delimited
            2 => {
                let Some((len, m)) = read_varint(&bytes[pos..]) else {
                    return;
                };
                pos += m;
                // A crafted length must not wrap pos or index past the end; reject
                // anything that overflows usize or exceeds the remaining bytes.
                let Ok(len) = usize::try_from(len) else {
                    return;
                };
                let Some(content) = pos.checked_add(len).and_then(|end| bytes.get(pos..end)) else {
                    return;
                };
                let content_base = base + pos;
                path.push(field);
                if looks_like_message(content) {
                    walk_fields(content, content_base, path, best);
                } else if let Some(off) = first_invalid_utf8_offset(content) {
                    let absolute = content_base + off;
                    if best.as_ref().is_none_or(|(_, b)| absolute < *b) {
                        *best = Some((path.clone(), absolute));
                    }
                }
                path.pop();
                pos += len;
            }
            // 32-bit
            5 => pos += 4,
            // groups (3/4) and anything else: stop, the frame is unparseable
            _ => return,
        }
        if pos > bytes.len() {
            return;
        }
    }
}

/// The byte offset of the first invalid UTF-8 byte, or `None` when valid.
fn first_invalid_utf8_offset(content: &[u8]) -> Option<usize> {
    std::str::from_utf8(content).err().map(|e| e.valid_up_to())
}

/// Whether `content` scans cleanly as a sequence of protobuf fields consuming
/// exactly all its bytes — the heuristic for recursing rather than UTF-8-checking.
fn looks_like_message(content: &[u8]) -> bool {
    if content.is_empty() {
        return false;
    }
    let mut pos = 0;
    while pos < content.len() {
        let Some((tag, n)) = read_varint(&content[pos..]) else {
            return false;
        };
        pos += n;
        match tag & 0x7 {
            0 => match read_varint(&content[pos..]) {
                Some((_, m)) => pos += m,
                None => return false,
            },
            1 => pos += 8,
            2 => {
                let Some((len, m)) = read_varint(&content[pos..]) else {
                    return false;
                };
                pos += m;
                // A crafted length must not wrap pos into an endless rescan; reject
                // a field claiming more than the bytes that remain.
                let Ok(len) = usize::try_from(len) else {
                    return false;
                };
                if len > content.len() - pos {
                    return false;
                }
                pos += len;
            }
            5 => pos += 4,
            _ => return false,
        }
    }
    pos == content.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{JobSpec, SubmitJobRequest};

    fn submit_with_script(script: &[u8]) -> Vec<u8> {
        let req = SubmitJobRequest {
            spec: Some(JobSpec {
                script: String::from_utf8_lossy(script).into_owned(),
                ..Default::default()
            }),
        };
        req.encode_to_vec()
    }

    #[test]
    fn a_flipped_script_byte_is_located_by_field_path_and_offset() {
        let marker = b"AAAABBBBCCCC_marker_DDDD";
        let mut bytes = submit_with_script(marker);
        // Flip the marker's 'C' run to an invalid UTF-8 byte.
        let at = bytes
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("script bytes present verbatim in the ASCII encoding")
            + 8;
        bytes[at] = 0xff;

        let (path, offset) = first_invalid_utf8(&bytes).expect("an invalid byte is located");
        assert_eq!(offset, at, "offset points at the flipped byte");
        // SubmitJobRequest.spec (field 1) -> JobSpec.script (field 20).
        assert_eq!(path, vec![1, 20], "path descends through spec into script");
    }

    #[test]
    fn a_valid_message_has_no_fault_and_round_trips() {
        let bytes = submit_with_script(b"#!/bin/bash\necho hi\n");
        // The decoder hands these exact bytes to prost (zero-copy), so a clean
        // message decodes identically and the locator finds nothing.
        let decoded = SubmitJobRequest::decode(bytes.as_slice()).expect("prost decodes it");
        assert_eq!(decoded.encode_to_vec(), bytes);
        assert!(
            first_invalid_utf8(&bytes).is_none(),
            "clean ASCII has no fault"
        );
    }

    #[test]
    fn a_truncated_message_errors_without_panicking() {
        let mut bytes = submit_with_script(b"#!/bin/bash\n");
        bytes.truncate(bytes.len() - 1);
        // The walker must not panic on a frame it cannot fully parse.
        let _ = first_invalid_utf8(&bytes);
        assert!(
            SubmitJobRequest::decode(bytes.as_slice()).is_err(),
            "a truncated frame fails to decode"
        );
    }

    #[test]
    fn plain_ascii_is_not_mistaken_for_a_fault() {
        assert!(first_invalid_utf8_offset(b"hello world").is_none());
        assert!(!looks_like_message(b"hello world"));
    }

    /// Wrap `leaf` in `depth` nested single length-delimited fields (field 1).
    fn nested_message(depth: usize, leaf: &[u8]) -> Vec<u8> {
        let mut buf = leaf.to_vec();
        for _ in 0..depth {
            let mut outer = Vec::with_capacity(buf.len() + MAX_VARINT_BYTES + 1);
            outer.push(0x0a); // field 1, length-delimited
            let mut len = buf.len() as u64;
            loop {
                let mut byte = (len & 0x7f) as u8;
                len >>= 7;
                if len != 0 {
                    byte |= 0x80;
                }
                outer.push(byte);
                if len == 0 {
                    break;
                }
            }
            outer.extend_from_slice(&buf);
            buf = outer;
        }
        buf
    }

    #[test]
    fn deeply_nested_input_is_bounded_not_followed_to_the_bottom() {
        // Fields nested far past the cap with an invalid-UTF-8 leaf at the bottom:
        // the walker must stop at the cap and report no fault, not recurse to the
        // leaf (unbounded, that risks a stack overflow on crafted input).
        let deep = nested_message(MAX_DIAGNOSTIC_DEPTH + 50, b"\xff");
        assert!(first_invalid_utf8(&deep).is_none());
    }

    #[test]
    fn an_oversized_length_is_rejected_not_wrapped() {
        // field 1 (length-delimited) claiming u64::MAX bytes that are not there:
        // both scanners must reject it, not wrap pos into an endless rescan.
        let mut buf = vec![0x0a];
        buf.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
        assert!(!looks_like_message(&buf));
        assert!(first_invalid_utf8(&buf).is_none());
    }

    #[test]
    fn the_status_keeps_prost_error_and_labels_the_offset_a_candidate() {
        // A non-UTF-8 byte the walker flags is only a guess at the fault, so the
        // status must surface prost's error and call the offset a candidate rather
        // than asserting invalid UTF-8 is the cause (it may be a valid bytes field).
        let marker = b"AAAABBBBCCCC";
        let mut bytes = submit_with_script(marker);
        let at = bytes
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("script bytes present")
            + 4;
        bytes[at] = 0xff;
        let err =
            SubmitJobRequest::decode(bytes.as_slice()).expect_err("invalid UTF-8 fails decode");
        let err_text = err.to_string();
        let message = describe_decode_failure::<SubmitJobRequest>(&bytes, &err)
            .message()
            .to_string();
        assert!(
            message.contains(&err_text),
            "status keeps prost's error: {message}"
        );
        assert!(
            message.contains(DECODE_FAILURE_MARKER),
            "status carries the recognisable decode-failure marker: {message}"
        );
        assert!(
            message.contains("candidate"),
            "offset is a labeled candidate: {message}"
        );
    }
}
