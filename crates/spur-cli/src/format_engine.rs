/// Slurm-compatible format string engine.
///
/// Parses format strings like `"%.18i %.9P %.8j %.8u %.2t %10M %6D %R"`
/// where each specifier is `%[flags][width][.precision]<letter>`.
///
/// The engine is generic: callers provide a mapping from format letter
/// to field value via a closure.

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

/// Parse a Slurm format string into fields.
///
/// Format: `%[.][width]<spec>` where:
/// - Leading `.` means truncate to width
/// - No `.` means pad to width
/// - Negative width means left-align
pub fn parse_format(fmt: &str, header_map: &dyn Fn(char) -> &'static str) -> Vec<FormatField> {
    let mut fields = Vec::new();
    let mut chars = fmt.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '%' {
            continue;
        }
        // Check for %%
        if chars.peek() == Some(&'%') {
            chars.next();
            continue;
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

        fields.push(FormatField {
            spec,
            width: abs_width,
            right_align,
            truncate: if truncate && has_width {
                Some(abs_width)
            } else {
                None
            },
            header,
        });
    }

    fields
}

/// Format a single row using parsed fields and a value resolver.
pub fn format_row(fields: &[FormatField], resolver: &dyn Fn(char) -> String) -> String {
    let mut parts = Vec::new();

    for field in fields {
        let value = resolver(field.spec);
        let formatted = format_field(&value, field);
        parts.push(formatted);
    }

    // Join with space and trim trailing whitespace
    parts.join(" ").trim_end().to_string()
}

/// Format the header row.
pub fn format_header(fields: &[FormatField]) -> String {
    let mut parts = Vec::new();

    for field in fields {
        let formatted = format_field(&field.header, field);
        parts.push(formatted);
    }

    parts.join(" ").trim_end().to_string()
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
        let fields = parse_format(SQUEUE_DEFAULT_FORMAT, &squeue_header);
        assert_eq!(fields.len(), 8);

        assert_eq!(fields[0].spec, 'i');
        assert_eq!(fields[0].width, 18);
        assert!(fields[0].truncate.is_some());

        assert_eq!(fields[1].spec, 'P');
        assert_eq!(fields[1].width, 9);
    }

    #[test]
    fn test_format_row() {
        let fields = parse_format("%.8i %.9P %.8j", &squeue_header);
        let row = format_row(&fields, &|spec| match spec {
            'i' => "12345".into(),
            'P' => "gpu".into(),
            'j' => "train".into(),
            _ => "?".into(),
        });
        assert_eq!(row, "   12345       gpu    train");
    }

    #[test]
    fn test_truncation() {
        let fields = parse_format("%.5j", &squeue_header);
        let row = format_row(&fields, &|spec| match spec {
            'j' => "very_long_job_name".into(),
            _ => "?".into(),
        });
        assert_eq!(row, "very_");
    }

    #[test]
    fn test_left_align() {
        let fields = parse_format("%-10j", &squeue_header);
        let row = format_row(&fields, &|spec| match spec {
            'j' => "short".into(),
            _ => "?".into(),
        });
        assert_eq!(row, "short");
    }

    #[test]
    fn test_header() {
        let fields = parse_format("%.18i %.9P %.8j", &squeue_header);
        let header = format_header(&fields);
        assert!(header.contains("JOBID"));
        assert!(header.contains("PARTITION"));
        assert!(header.contains("NAME"));
    }
}
