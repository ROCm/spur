// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Prefixes each line of one task's output with `[procid] `, byte for byte what
/// the bash wrapper's `sed "s/^/[N] /"` produced.
pub struct LineLabeler {
    prefix: Vec<u8>,
    partial: Vec<u8>,
}

/// A task that never writes a newline would otherwise grow the buffer without
/// bound, so a line this long is flushed in pieces.
const LINE_CAP: usize = 1 << 20;

impl LineLabeler {
    pub fn new(procid: u32) -> Self {
        Self {
            prefix: format!("[{procid}] ").into_bytes(),
            partial: Vec::new(),
        }
    }

    /// Append the labeled form of every complete line in `bytes` to `out`.
    pub fn feed(&mut self, mut bytes: &[u8], out: &mut Vec<u8>) {
        while let Some(newline) = bytes.iter().position(|&b| b == b'\n') {
            out.extend_from_slice(&self.prefix);
            out.append(&mut self.partial);
            out.extend_from_slice(&bytes[..=newline]);
            bytes = &bytes[newline + 1..];
        }
        self.partial.extend_from_slice(bytes);
        if self.partial.len() >= LINE_CAP {
            self.finish(out);
        }
    }

    /// Flush a final line that never got a newline. `sed` does not add one.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.partial.is_empty() {
            return;
        }
        out.extend_from_slice(&self.prefix);
        out.append(&mut self.partial);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(procid: u32, chunks: &[&str]) -> String {
        let mut labeler = LineLabeler::new(procid);
        let mut out = Vec::new();
        for chunk in chunks {
            labeler.feed(chunk.as_bytes(), &mut out);
        }
        labeler.finish(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn every_line_gets_the_prefix() {
        assert_eq!(label(3, &["a\nb\n"]), "[3] a\n[3] b\n");
    }

    #[test]
    fn lines_split_across_reads_are_joined() {
        assert_eq!(
            label(3, &["he", "llo\nwo", "rld\n"]),
            "[3] hello\n[3] world\n"
        );
    }

    #[test]
    fn empty_lines_are_labeled() {
        assert_eq!(label(0, &["\n"]), "[0] \n");
    }

    #[test]
    fn a_partial_last_line_is_labeled_without_a_newline() {
        assert_eq!(label(7, &["done"]), "[7] done");
    }

    #[test]
    fn no_output_writes_nothing() {
        assert_eq!(label(1, &[]), "");
    }

    #[test]
    fn a_line_past_the_cap_is_flushed_without_waiting_for_its_newline() {
        let mut labeler = LineLabeler::new(2);
        let mut out = Vec::new();
        labeler.feed(&vec![b'x'; LINE_CAP], &mut out);
        assert_eq!(out.len(), "[2] ".len() + LINE_CAP);
        assert!(out.starts_with(b"[2] x"));
    }
}
