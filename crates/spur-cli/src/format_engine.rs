// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Slurm-compatible format string engine.
//!
//! Parses format strings like `"%.18i %.9P %.8j %.8u %.2t %10M %6D %R"`
//! where each specifier is `%[flags][width][.precision]<letter>`.
//!
//! Non-specifier characters (spaces, commas, etc.) are preserved as literal
//! tokens so that `%k,%A` emits `comment,jobid` with no extra spacing.

/// A parsed format field.
#[derive(Debug, Clone)]
pub struct FormatField {
    /// The format specifier character (e.g., 'i' for job ID).
    pub spec: char,
    /// Minimum field width. 0 means no minimum.
    pub width: usize,
    /// Right-align (default) vs left-align.
    pub right_align: bool,
    /// Truncate to this width (via leading dot, e.g., %.18i).
    pub truncate: Option<usize>,
    /// The column header for this field.
    pub header: String,
}

/// A token in a parsed format string: either a field specifier or literal text.
#[derive(Debug, Clone)]
pub enum FormatToken {
    Field(FormatField),
    Literal(String),
}

/// Parse a Slurm format string into tokens (fields + literal runs).
///
/// Format: `%[.][width]<spec>` where:
/// - Leading `.` means truncate to width
/// - No `.` means pad to width
/// - Negative width means left-align
/// - `%%` produces a literal `%`
/// - Any characters between specifiers are preserved as `Literal` tokens
pub fn parse_format(fmt: &str, header_map: &dyn Fn(char) -> &'static str) -> Vec<FormatToken> {
    let mut tokens = Vec::new();
    let mut chars = fmt.chars().peekable();
    let mut literal_buf = String::new();

    while let Some(c) = chars.next() {
        if c != '%' {
            literal_buf.push(c);
            continue;
        }

        // Check for %%
        if chars.peek() == Some(&'%') {
            chars.next();
            literal_buf.push('%');
            continue;
        }

        // Flush accumulated literal text before parsing the field
        if !literal_buf.is_empty() {
            tokens.push(FormatToken::Literal(std::mem::take(&mut literal_buf)));
        }

        let mut truncate = false;
        let mut width: i32 = 0;
        let mut has_width = false;
        let mut hash_flag = false;

        // Leading # = variable-width, left-aligned (no padding/truncation)
        if chars.peek() == Some(&'#') {
            hash_flag = true;
            chars.next();
        }

        // Leading dot = truncate mode
        if chars.peek() == Some(&'.') {
            truncate = true;
            chars.next();
        }

        // Width (possibly negative for left-align)
        let mut negative = chars.peek() == Some(&'-');
        if negative {
            chars.next();
        }

        if hash_flag {
            negative = true; // # implies left-align
        }

        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                has_width = true;
                width = width * 10 + (d as i32 - '0' as i32);
                chars.next();
            } else {
                break;
            }
        }

        if negative {
            width = -width;
        }

        // Spec character
        let spec = match chars.next() {
            Some(s) => s,
            None => break,
        };

        let abs_width = width.unsigned_abs() as usize;
        let right_align = if hash_flag { false } else { width >= 0 };
        let header = header_map(spec).to_string();

        tokens.push(FormatToken::Field(FormatField {
            spec,
            width: abs_width,
            right_align,
            truncate: if truncate && has_width {
                Some(abs_width)
            } else {
                None
            },
            header,
        }));
    }

    // Flush trailing literal
    if !literal_buf.is_empty() {
        tokens.push(FormatToken::Literal(literal_buf));
    }

    tokens
}

/// Parse sacct/sreport-style comma-separated field names, unlike squeue/sinfo's `%`-specifiers.
pub fn parse_named_format(
    fmt: &str,
    name_to_spec: &dyn Fn(&str) -> Option<char>,
    header_map: &dyn Fn(char) -> &'static str,
) -> Vec<FormatToken> {
    let mut tokens = Vec::new();
    let items: Vec<&str> = fmt.split(',').collect();
    for (i, item) in items.iter().enumerate() {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (name, width) = match item.split_once('%') {
            Some((n, w)) => (n.trim(), w.trim().parse::<i32>().unwrap_or(0)),
            None => (item, 0),
        };
        let Some(spec) = name_to_spec(name) else {
            continue;
        };
        let abs_width = width.unsigned_abs() as usize;
        tokens.push(FormatToken::Field(FormatField {
            spec,
            width: abs_width,
            right_align: width >= 0,
            truncate: if abs_width > 0 { Some(abs_width) } else { None },
            header: header_map(spec).to_string(),
        }));
        if i + 1 < items.len() {
            tokens.push(FormatToken::Literal(" ".to_string()));
        }
    }
    tokens
}

/// Format a single row using parsed tokens and a value resolver.
pub fn format_row(tokens: &[FormatToken], resolver: &dyn Fn(char) -> String) -> String {
    let mut out = String::new();

    for token in tokens {
        match token {
            FormatToken::Field(field) => {
                let value = resolver(field.spec);
                out.push_str(&format_field(&value, field));
            }
            FormatToken::Literal(lit) => out.push_str(lit),
        }
    }

    out.trim_end().to_string()
}

/// Format the header row.
pub fn format_header(tokens: &[FormatToken]) -> String {
    let mut out = String::new();

    for token in tokens {
        match token {
            FormatToken::Field(field) => {
                out.push_str(&format_field(&field.header, field));
            }
            FormatToken::Literal(lit) => out.push_str(lit),
        }
    }

    out.trim_end().to_string()
}

/// Format a single field value with width/alignment/truncation.
fn format_field(value: &str, field: &FormatField) -> String {
    let mut s = value.to_string();

    // Truncate if specified
    if let Some(max) = field.truncate {
        if s.len() > max {
            s.truncate(max);
        }
    }

    // Pad to width
    if field.width > 0 {
        if field.right_align {
            format!("{:>width$}", s, width = field.width)
        } else {
            format!("{:<width$}", s, width = field.width)
        }
    } else {
        s
    }
}

/// Print a header row followed by a separator line (dashes under text, spaces under spaces).
pub fn print_header(tokens: &[FormatToken]) {
    let header = format_header(tokens);
    let sep: String = header
        .chars()
        .map(|c| if c == ' ' { ' ' } else { '-' })
        .collect();
    println!("{header}");
    println!("{sep}");
}

/// Resolve a `format=` parameter into tokens.
///
/// - `"all"` (case-insensitive) expands `all_format` via `parse_format`.
/// - Any other value is parsed as comma-separated field names via `parse_named_format`.
/// - Returns an error if the value contains no recognized field names (with `valid_hint`).
/// - `None` falls back to `default_format` via `parse_format`.
pub fn resolve_format(
    format_param: Option<&str>,
    default_format: &str,
    all_format: &str,
    name_to_spec: &dyn Fn(&str) -> Option<char>,
    header_map: &dyn Fn(char) -> &'static str,
    valid_hint: &str,
) -> anyhow::Result<Vec<FormatToken>> {
    match format_param {
        Some(fmt) if fmt.eq_ignore_ascii_case("all") => Ok(parse_format(all_format, header_map)),
        Some(fmt) => {
            let f = parse_named_format(fmt, name_to_spec, header_map);
            let has_fields = f.iter().any(|t| matches!(t, FormatToken::Field(_)));
            if !has_fields {
                anyhow::bail!("no recognized fields in format='{fmt}'. Valid fields: {valid_hint}");
            }
            Ok(f)
        }
        None => Ok(parse_format(default_format, header_map)),
    }
}

/// Default squeue format string (matches Slurm default).
pub const SQUEUE_DEFAULT_FORMAT: &str = "%.18i %.9P %.8j %.8u %.2t %10M %6D %R";

/// Default sinfo format string.
pub const SINFO_DEFAULT_FORMAT: &str = "%#P %5a %.10l %.6D %.6t %N";

/// Header names for squeue format specifiers.
pub fn squeue_header(spec: char) -> &'static str {
    match spec {
        'i' => "JOBID",
        'j' => "NAME",
        'u' => "USER",
        'P' => "PARTITION",
        't' => "ST",
        'T' => "STATE",
        'M' => "TIME",
        'l' => "TIME_LIMIT",
        'D' => "NODES",
        'R' => "NODELIST(REASON)",
        'C' => "CPUS",
        'a' => "ACCOUNT",
        'p' => "PRIORITY",
        'S' => "START_TIME",
        'V' => "SUBMIT_TIME",
        'e' => "END_TIME",
        'Z' => "WORK_DIR",
        'o' => "COMMAND",
        'q' => "QOS",
        'r' => "REASON",
        'n' => "NAME",
        'N' => "NODELIST",
        'b' => "GRES",
        'L' => "TIME_LEFT",
        'v' => "RESERVATION",
        'k' => "COMMENT",
        'A' => "ARRAY_JOB_ID",
        _ => "?",
    }
}

/// Header names for sinfo format specifiers.
pub fn sinfo_header(spec: char) -> &'static str {
    match spec {
        'P' => "PARTITION",
        'a' => "AVAIL",
        'l' => "TIMELIMIT",
        'D' => "NODES",
        't' => "STATE",
        'T' => "STATE",
        'N' => "NODELIST",
        'C' => "CPUS(A/I/O/T)",
        'c' => "CPUS",
        'm' => "MEMORY",
        'f' => "AVAIL_FEATURES",
        'G' => "GRES",
        'R' => "PARTITION",
        'n' => "HOSTNAMES",
        'O' => "CPU_LOAD",
        'e' => "FREE_MEM",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_default_squeue_format() {
        let tokens = parse_format(SQUEUE_DEFAULT_FORMAT, &squeue_header);
        let field_count = tokens
            .iter()
            .filter(|t| matches!(t, FormatToken::Field(_)))
            .count();
        assert_eq!(field_count, 8);

        let first = match &tokens[0] {
            FormatToken::Field(f) => f,
            _ => panic!("expected field"),
        };
        assert_eq!(first.spec, 'i');
        assert_eq!(first.width, 18);
        assert!(first.truncate.is_some());
    }

    #[test]
    fn test_format_row() {
        let tokens = parse_format("%.8i %.9P %.8j", &squeue_header);
        let row = format_row(&tokens, &|spec| match spec {
            'i' => "12345".into(),
            'P' => "gpu".into(),
            'j' => "train".into(),
            _ => "?".into(),
        });
        assert_eq!(row, "   12345       gpu    train");
    }

    #[test]
    fn test_truncation() {
        let tokens = parse_format("%.5j", &squeue_header);
        let row = format_row(&tokens, &|spec| match spec {
            'j' => "very_long_job_name".into(),
            _ => "?".into(),
        });
        assert_eq!(row, "very_");
    }

    #[test]
    fn test_left_align() {
        let tokens = parse_format("%-10j", &squeue_header);
        let row = format_row(&tokens, &|spec| match spec {
            'j' => "short".into(),
            _ => "?".into(),
        });
        assert_eq!(row, "short");
    }

    #[test]
    fn test_header() {
        let tokens = parse_format("%.18i %.9P %.8j", &squeue_header);
        let header = format_header(&tokens);
        assert!(header.contains("JOBID"));
        assert!(header.contains("PARTITION"));
        assert!(header.contains("NAME"));
    }

    #[test]
    fn test_literal_comma_preserved() {
        let tokens = parse_format("%k,%A", &squeue_header);
        assert_eq!(tokens.len(), 3);
        assert!(matches!(&tokens[0], FormatToken::Field(f) if f.spec == 'k'));
        assert!(matches!(&tokens[1], FormatToken::Literal(s) if s == ","));
        assert!(matches!(&tokens[2], FormatToken::Field(f) if f.spec == 'A'));

        let row = format_row(&tokens, &|spec| match spec {
            'k' => "mycomment".into(),
            'A' => "42".into(),
            _ => "?".into(),
        });
        assert_eq!(row, "mycomment,42");
    }

    #[test]
    fn test_percent_percent_becomes_literal() {
        let tokens = parse_format("%%done", &squeue_header);
        assert_eq!(tokens.len(), 1);
        assert!(matches!(&tokens[0], FormatToken::Literal(s) if s == "%done"));
    }

    #[test]
    fn test_default_format_spacing_unchanged() {
        let tokens = parse_format("%.8i %.9P", &squeue_header);
        let row = format_row(&tokens, &|spec| match spec {
            'i' => "1".into(),
            'P' => "gpu".into(),
            _ => "?".into(),
        });
        assert_eq!(row, "       1       gpu");
    }

    fn test_name_to_spec(name: &str) -> Option<char> {
        match name.to_lowercase().as_str() {
            "jobid" => Some('i'),
            "partition" => Some('P'),
            "jobname" => Some('j'),
            _ => None,
        }
    }

    #[test]
    fn parse_named_format_comma_separated_names() {
        let tokens = parse_named_format(
            "JobID,Partition,JobName",
            &test_name_to_spec,
            &squeue_header,
        );
        let field_count = tokens
            .iter()
            .filter(|t| matches!(t, FormatToken::Field(_)))
            .count();
        assert_eq!(field_count, 3);
        let specs: Vec<char> = tokens
            .iter()
            .filter_map(|t| match t {
                FormatToken::Field(f) => Some(f.spec),
                _ => None,
            })
            .collect();
        assert_eq!(specs, vec!['i', 'P', 'j']);
    }

    #[test]
    fn parse_named_format_applies_percent_width() {
        let tokens = parse_named_format("JobName%20", &test_name_to_spec, &squeue_header);
        let field = match &tokens[0] {
            FormatToken::Field(f) => f,
            _ => panic!("expected field"),
        };
        assert_eq!(field.width, 20);
        assert!(field.right_align);
        assert_eq!(field.truncate, Some(20));
    }

    #[test]
    fn parse_named_format_negative_width_left_aligns() {
        let tokens = parse_named_format("JobName%-20", &test_name_to_spec, &squeue_header);
        let field = match &tokens[0] {
            FormatToken::Field(f) => f,
            _ => panic!("expected field"),
        };
        assert_eq!(field.width, 20);
        assert!(!field.right_align);
    }

    #[test]
    fn parse_named_format_skips_unknown_field_names() {
        let tokens = parse_named_format(
            "JobID,NotAField,Partition",
            &test_name_to_spec,
            &squeue_header,
        );
        let field_count = tokens
            .iter()
            .filter(|t| matches!(t, FormatToken::Field(_)))
            .count();
        assert_eq!(field_count, 2);
    }

    #[test]
    fn parse_named_format_is_case_insensitive() {
        let tokens = parse_named_format("jobid,PARTITION", &test_name_to_spec, &squeue_header);
        let field_count = tokens
            .iter()
            .filter(|t| matches!(t, FormatToken::Field(_)))
            .count();
        assert_eq!(field_count, 2);
    }
}
