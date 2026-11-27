// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use tracing::{Level, Subscriber};
use tracing_subscriber::filter::DynFilterFn;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Name of the span that wraps the apply of every entry that was on disk at
/// startup. `spurctld` drops INFO and below inside it, because a restart
/// re-applies the whole log and would report old events as new. WARN and ERROR
/// pass, because a bad transition during a replay is a defect.
pub const WAL_REPLAY_SPAN: &str = "wal_replay";

pub fn init(log_level: &str) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| log_level.parse().unwrap());
    subscriber(env_filter, std::io::stdout).init();
}

fn subscriber<W>(env_filter: EnvFilter, writer: W) -> impl Subscriber + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::registry().with(env_filter).with(
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_filter(wal_replay_filter()),
    )
}

/// Drops INFO and below under [`WAL_REPLAY_SPAN`]; WARN and ERROR pass.
fn wal_replay_filter<S>() -> DynFilterFn<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    DynFilterFn::new(|meta, cx: &Context<'_, S>| {
        if *meta.level() <= Level::WARN {
            return true;
        }
        cx.lookup_current()
            .is_none_or(|span| !span.scope().any(|s| s.name() == WAL_REPLAY_SPAN))
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Runs `f` under the production subscriber stack with a capturing writer.
    /// The global subscriber is untouched, so tests can run in parallel.
    fn capture(f: impl FnOnce()) -> String {
        let out = Captured::default();
        let sink = out.clone();
        let sub = subscriber(EnvFilter::new("info"), move || sink.clone());
        tracing::subscriber::with_default(sub, f);
        out.text()
    }

    #[test]
    fn info_inside_wal_replay_span_is_dropped() {
        let out = capture(|| {
            let _g = tracing::info_span!(WAL_REPLAY_SPAN, index = 3u64).entered();
            tracing::info!("replayed-info");
        });
        assert!(!out.contains("replayed-info"), "{out}");
    }

    #[test]
    fn warn_and_error_inside_wal_replay_span_are_kept() {
        let out = capture(|| {
            let _g = tracing::info_span!(WAL_REPLAY_SPAN, index = 3u64).entered();
            tracing::warn!("replayed-warn");
            tracing::error!("replayed-error");
        });
        assert!(out.contains("replayed-warn"), "{out}");
        assert!(out.contains("replayed-error"), "{out}");
    }

    #[test]
    fn info_outside_wal_replay_span_is_kept() {
        let out = capture(|| {
            tracing::info!("live-before");
            {
                let _g = tracing::info_span!(WAL_REPLAY_SPAN, index = 3u64).entered();
                tracing::info!("replayed-info");
            }
            tracing::info!("live-after");
        });
        assert!(out.contains("live-before"), "{out}");
        assert!(out.contains("live-after"), "{out}");
        assert!(!out.contains("replayed-info"), "{out}");
    }

    #[test]
    fn info_in_child_span_of_wal_replay_is_dropped() {
        let out = capture(|| {
            let _g = tracing::info_span!(WAL_REPLAY_SPAN, index = 3u64).entered();
            let _child = tracing::info_span!("apply_job", job_id = 7u64).entered();
            tracing::info!("nested-info");
            tracing::warn!("nested-warn");
        });
        assert!(!out.contains("nested-info"), "{out}");
        assert!(out.contains("nested-warn"), "{out}");
    }

    #[test]
    fn info_in_unrelated_span_is_kept() {
        let out = capture(|| {
            let _g = tracing::info_span!("scheduler_tick").entered();
            tracing::info!("tick-info");
        });
        assert!(out.contains("tick-info"), "{out}");
    }

    #[test]
    fn env_filter_still_applies_outside_replay() {
        let out = capture(|| {
            tracing::debug!("debug-outside");
            tracing::info!("info-outside");
        });
        assert!(!out.contains("debug-outside"), "{out}");
        assert!(out.contains("info-outside"), "{out}");
    }
}
