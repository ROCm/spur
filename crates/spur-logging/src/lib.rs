// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared logging setup for Spur binaries.
//!
//! Every binary (`spurctld`, `spurd`, `spur-k8s-operator`, `spur`) installs the
//! same subscriber through [`init`] or [`init_cli`], so a single schema and a
//! single set of precedence rules govern all of them instead of four
//! independent `tracing_subscriber::fmt()` setups drifting apart.
//!
//! JSON lines carry a fixed schema: `timestamp` (RFC 3339, nanoseconds),
//! `level` (lowercase), `component`, `target` (Rust module path), `message`,
//! and any structured fields the call site attached (`job_id`, `node`,
//! `error`, ...). Values keep their native JSON type, so numbers stay numbers.

use std::fmt;
use std::io::IsTerminal;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

/// Output format for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Text,
}

/// A logging value that could not be resolved into a subscriber.
///
/// Surfaced from [`init`] so a daemon fails fast on a bad `--log-level` /
/// `--log-format` or `[logging]` value instead of starting up silent or in the
/// wrong format.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[error("invalid log level/filter {value:?}: {reason}")]
    InvalidFilter { value: String, reason: String },
    #[error("invalid log format {value:?} (expected \"json\" or \"text\")")]
    InvalidFormat { value: String },
}

/// Install the global subscriber for a daemon.
///
/// `component` is stamped onto every line. `cli_level` / `cli_format` are the
/// `--log-level` / `--log-format` flag values (`None` when unset); `config_*`
/// are the `[logging]` values (empty string when unset). Format is chosen by
/// the TTY rule when neither flag nor config sets it.
pub fn init(
    component: &'static str,
    cli_level: Option<&str>,
    cli_format: Option<&str>,
    config_level: &str,
    config_format: &str,
) -> Result<(), LoggingError> {
    let filter = resolve_filter(cli_level, config_level, "info")?;
    let format = resolve_format(cli_format, config_format, std::io::stderr().is_terminal())?;
    install(component, filter, format, false);
    Ok(())
}

/// Install the global subscriber for the CLI.
///
/// The CLI ignores `spur.conf` for logging and defaults to `warn` so library
/// diagnostics stay quiet next to the command's own output. Output is always
/// compact text: the CLI writes its results to stdout and its own errors to
/// stderr as plain text, so emitting JSON tracing lines on stderr would mix
/// formats for a script capturing it. `RUST_LOG` still tunes the level.
pub fn init_cli(component: &'static str) {
    // Inputs are constant and valid, so resolution cannot fail here; fall back
    // to `warn` rather than unwrap on the impossible error.
    let filter = resolve_filter(None, "", "warn").unwrap_or_else(|_| EnvFilter::new("warn"));
    install(component, filter, Format::Text, true);
}

fn install(component: &'static str, filter: EnvFilter, format: Format, compact_text: bool) {
    match format {
        Format::Json => {
            let layer = tracing_subscriber::fmt::layer()
                .event_format(SpurJsonFormat::new(component))
                .with_writer(std::io::stderr);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(layer)
                .try_init();
        }
        Format::Text if compact_text => {
            // The CLI's own output is on stdout; keep tracing noise minimal.
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .without_time()
                .with_target(false)
                .try_init();
        }
        Format::Text => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();
        }
    }
}

/// Resolve the level string with precedence flag > config > default. The
/// `RUST_LOG` tier is layered on top in [`resolve_filter`]; keeping this pure
/// (no env, no I/O) makes the precedence unit-testable.
fn resolve_level(cli_level: Option<&str>, config_level: &str, default_level: &str) -> String {
    if let Some(level) = cli_level {
        if !level.is_empty() {
            return level.to_string();
        }
    }
    if !config_level.is_empty() {
        return config_level.to_string();
    }
    default_level.to_string()
}

fn resolve_filter(
    cli_level: Option<&str>,
    config_level: &str,
    default_level: &str,
) -> Result<EnvFilter, LoggingError> {
    // RUST_LOG (including per-module directives) wins when present and valid.
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        return Ok(filter);
    }
    parse_filter(&resolve_level(cli_level, config_level, default_level))
}

/// Build an `EnvFilter` from a resolved level/directive string, rejecting a
/// bare token that is not a real level. `EnvFilter` otherwise accepts e.g.
/// `notalevel` as a per-target directive matching nothing, so the process
/// starts and emits zero log lines. Pure (no env), so it is deterministically
/// testable.
fn parse_filter(level: &str) -> Result<EnvFilter, LoggingError> {
    // A single bare token (no per-target directives) must name a valid level;
    // strings with `=`/`,` are RUST_LOG-style directives and parse as-is.
    if !level.contains('=') && !level.contains(',') && level.parse::<LevelFilter>().is_err() {
        return Err(LoggingError::InvalidFilter {
            value: level.to_string(),
            reason: "expected one of trace, debug, info, warn, error".to_string(),
        });
    }
    match level.parse::<EnvFilter>() {
        Ok(filter) => Ok(filter),
        Err(e) => Err(LoggingError::InvalidFilter {
            value: level.to_string(),
            reason: e.to_string(),
        }),
    }
}

fn resolve_format(
    cli_format: Option<&str>,
    config_format: &str,
    is_tty: bool,
) -> Result<Format, LoggingError> {
    let config_choice = if config_format.is_empty() {
        None
    } else {
        Some(config_format)
    };
    let choice = cli_format.filter(|s| !s.is_empty()).or(config_choice);
    match choice {
        Some("json") => Ok(Format::Json),
        Some("text") => Ok(Format::Text),
        // An explicit but unrecognized value is a typo, not a request for the
        // TTY default — reject it so it can't silently flip text/JSON by env.
        Some(other) => Err(LoggingError::InvalidFormat {
            value: other.to_owned(),
        }),
        None if is_tty => Ok(Format::Text),
        None => Ok(Format::Json),
    }
}

fn level_str(level: &Level) -> &'static str {
    match *level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
}

/// Event formatter that writes one flat JSON object per line.
struct SpurJsonFormat {
    component: &'static str,
}

impl SpurJsonFormat {
    fn new(component: &'static str) -> Self {
        Self { component }
    }
}

impl<S, N> FormatEvent<S, N> for SpurJsonFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let mut map = serde_json::Map::new();
        map.insert(
            "timestamp".to_string(),
            chrono::Utc::now()
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                .into(),
        );
        map.insert("level".to_string(), level_str(meta.level()).into());
        map.insert("component".to_string(), self.component.into());
        map.insert("target".to_string(), meta.target().into());

        event.record(&mut JsonVisitor(&mut map));

        // `message` is required on every line even for field-only events.
        map.entry("message")
            .or_insert_with(|| serde_json::Value::String(String::new()));

        let line = serde_json::to_string(&map).map_err(|_| fmt::Error)?;
        writeln!(writer, "{line}")
    }
}

/// Records `tracing` field values into a JSON map, keeping native types.
struct JsonVisitor<'a>(&'a mut serde_json::Map<String, serde_json::Value>);

impl JsonVisitor<'_> {
    fn insert(&mut self, field: &Field, value: serde_json::Value) {
        let name = field.name();
        // The formatter writes `timestamp`/`level`/`component`/`target` before
        // recording fields, so a call-site field with one of those names would
        // otherwise overwrite the canonical value and silently break the fixed
        // schema (e.g. `target = %node` turning `target` into a hostname). Rename
        // the collision to `field_<name>` so both survive. `message` is not
        // renamed: the event's own message is recorded as a field named
        // `message`, so it must keep that key — call sites that also pass an
        // explicit `message` field are renamed at the source instead.
        let key = match name {
            "timestamp" | "level" | "component" | "target" => format!("field_{name}"),
            _ => name.to_string(),
        };
        self.0.insert(key, value);
    }
}

impl Visit for JsonVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, value.into());
    }
    fn record_i128(&mut self, field: &Field, value: i128) {
        // JSON has no 128-bit integers; stringify to avoid lossy conversion.
        self.insert(field, value.to_string().into());
    }
    fn record_u128(&mut self, field: &Field, value: u128) {
        self.insert(field, value.to_string().into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, value.into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, value.into());
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, value.to_string().into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // Covers the event `message` and any `?`/`%` fields. `fmt::Arguments`
        // (the message) renders via Display, so this yields the plain string.
        self.insert(field, format!("{value:?}").into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct MockWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for MockWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MockWriter {
        type Writer = MockWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_json<F: FnOnce()>(component: &'static str, emit: F) -> serde_json::Value {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let layer = tracing_subscriber::fmt::layer()
            .event_format(SpurJsonFormat::new(component))
            .with_writer(MockWriter(buf.clone()));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, emit);
        let bytes = buf.lock().unwrap().clone();
        let text = String::from_utf8(bytes).unwrap();
        let line = text.lines().next().expect("expected a log line");
        serde_json::from_str(line).expect("log line must be valid JSON")
    }

    #[test]
    fn json_has_required_fields_and_flat_context() {
        let v = capture_json("spurctld", || {
            tracing::info!(job_id = 42u64, "job submitted");
        });
        assert_eq!(v["level"], "info");
        assert_eq!(v["component"], "spurctld");
        assert_eq!(v["message"], "job submitted");
        assert_eq!(v["job_id"], 42);
        assert!(v["timestamp"].is_string());
        assert!(v["target"].as_str().unwrap().contains("spur_logging"));
    }

    #[test]
    fn error_kind_is_a_string_field() {
        let v = capture_json("spurd", || {
            tracing::error!(job_id = 7u64, error = "accounting_start_failed", "boom");
        });
        assert_eq!(v["level"], "error");
        assert_eq!(v["error"], "accounting_start_failed");
        assert_eq!(v["job_id"], 7);
    }

    #[test]
    fn warn_level_is_lowercase() {
        let v = capture_json("spurd", || {
            tracing::warn!(node = "gpu-07", "node drained");
        });
        assert_eq!(v["level"], "warn");
        assert_eq!(v["node"], "gpu-07");
    }

    #[test]
    fn message_present_for_field_only_event() {
        let v = capture_json("spurctld", || {
            tracing::info!(job_id = 1u64);
        });
        assert!(v.get("message").is_some());
    }

    #[test]
    fn quotes_and_newlines_stay_one_valid_object() {
        let v = capture_json("spurctld", || {
            tracing::info!(
                detail = "he said \"hi\"\nthen left",
                "weird \"message\"\nsecond line"
            );
        });
        assert_eq!(v["message"], "weird \"message\"\nsecond line");
        assert_eq!(v["detail"], "he said \"hi\"\nthen left");
    }

    #[test]
    fn level_precedence() {
        assert_eq!(resolve_level(Some("debug"), "warn", "info"), "debug");
        assert_eq!(resolve_level(None, "warn", "info"), "warn");
        assert_eq!(resolve_level(Some(""), "warn", "info"), "warn");
        assert_eq!(resolve_level(None, "", "info"), "info");
    }

    #[test]
    fn reserved_keys_are_not_clobbered_by_event_fields() {
        let v = capture_json("spurd", || {
            tracing::warn!(target = "gpu-07", "node drained");
        });
        // The canonical `target` stays the module path; the colliding call-site
        // field is preserved under `field_target` instead of overwriting it.
        assert!(v["target"].as_str().unwrap().contains("spur_logging"));
        assert_eq!(v["field_target"], "gpu-07");
        // The event's own message still occupies the canonical `message` key.
        assert_eq!(v["message"], "node drained");
    }

    #[test]
    fn format_resolution() {
        assert_eq!(
            resolve_format(Some("json"), "text", true).unwrap(),
            Format::Json
        );
        assert_eq!(
            resolve_format(Some("text"), "json", false).unwrap(),
            Format::Text
        );
        assert_eq!(resolve_format(None, "json", true).unwrap(), Format::Json);
        assert_eq!(resolve_format(None, "text", false).unwrap(), Format::Text);
        assert_eq!(resolve_format(None, "", true).unwrap(), Format::Text);
        assert_eq!(resolve_format(None, "", false).unwrap(), Format::Json);
        assert_eq!(resolve_format(Some(""), "", false).unwrap(), Format::Json);
        // An explicit but unrecognized value is rejected, not silently defaulted
        // by the TTY rule.
        assert!(resolve_format(Some("jason"), "", true).is_err());
        assert!(resolve_format(None, "jsonn", false).is_err());
    }

    #[test]
    fn filter_rejects_bare_non_level() {
        // Valid bare levels and RUST_LOG-style directives parse.
        assert!(parse_filter("info").is_ok());
        assert!(parse_filter("debug").is_ok());
        assert!(parse_filter("spurctld=debug,spur_net=trace").is_ok());
        // A typo that EnvFilter would otherwise accept as a match-nothing target
        // directive (silencing the daemon) is rejected instead.
        assert!(parse_filter("notalevel").is_err());
        assert!(parse_filter("infoo").is_err());
    }
}
