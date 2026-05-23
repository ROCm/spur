// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OpenMetrics 1.0 text encoding helpers.

use std::fmt::Write;

/// Escape a label value per OpenMetrics / Prometheus exposition rules.
pub fn escape_label_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '"' => out.push_str(r#"\""#),
            _ => out.push(ch),
        }
    }
    out
}

/// Incremental OpenMetrics text builder.
#[derive(Debug, Default)]
pub struct OpenMetricsEncoder {
    body: String,
}

impl OpenMetricsEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn finish(self) -> String {
        self.body
    }

    pub fn write_gauge(&mut self, name: &str, help: &str, value: u64) {
        let _ = writeln!(self.body, "# HELP {name} {help}");
        let _ = writeln!(self.body, "# TYPE {name} gauge");
        let _ = writeln!(self.body, "{name} {value}");
    }

    pub fn write_gauge_with_labels(
        &mut self,
        name: &str,
        help: &str,
        value: u64,
        labels: &[(&str, &str)],
    ) {
        let _ = writeln!(self.body, "# HELP {name} {help}");
        let _ = writeln!(self.body, "# TYPE {name} gauge");
        if labels.is_empty() {
            let _ = writeln!(self.body, "{name} {value}");
            return;
        }
        let label_str = labels
            .iter()
            .map(|(k, v)| format!(r#"{k}="{}""#, escape_label_value(v)))
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(self.body, "{name}{{{label_str}}} {value}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_label_value_special_chars() {
        assert_eq!(escape_label_value(r#"a\b"#), r"a\\b");
        assert_eq!(escape_label_value("line\nbreak"), r"line\nbreak");
        assert_eq!(escape_label_value(r#"say "hi""#), r#"say \"hi\""#);
    }

    #[test]
    fn write_gauge_format() {
        let mut enc = OpenMetricsEncoder::new();
        enc.write_gauge("spur_jobs", "Total number of jobs", 42);
        assert_eq!(
            enc.finish(),
            "# HELP spur_jobs Total number of jobs\n\
             # TYPE spur_jobs gauge\n\
             spur_jobs 42\n"
        );
    }
}
