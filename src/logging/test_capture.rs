//! Scoped capture for collocated emitter tests; never installs global state.

use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(crate) struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("test log buffer").clone())
            .expect("UTF-8 tracing output")
    }
}

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("test log buffer")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Keeps one dispatcher alive for the whole test process.
///
/// `tracing` recomputes two process-global caches whenever a dispatcher is
/// created: one cached interest per callsite, and the maximum level filter that
/// every `event!` macro reads before it looks at a subscriber. A rebuild that
/// finds no other live dispatcher asks the *thread* default instead, which in a
/// parallel test binary is `NoSubscriber`: it caches `never` for the callsite and
/// stores `OFF` for the level filter, so a capture running on another thread
/// silently loses events it was built to see. This flaked in CI (`跳过频道`
/// missing from one ordering assertion) but never on a developer box with more
/// parallel dispatchers. A trace-level sink keeps both caches permissive for the
/// life of the process, so a scoped capture only ever answers for itself.
fn keepalive() {
    static KEEPALIVE: std::sync::LazyLock<tracing::Dispatch> = std::sync::LazyLock::new(|| {
        tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(std::io::sink)
                .finish(),
        )
    });
    std::sync::LazyLock::force(&KEEPALIVE);
}

pub(crate) fn subscriber(level: tracing::Level) -> (tracing::Dispatch, CapturedLogs) {
    // Before the scoped dispatch: its rebuild then counts two live dispatchers
    // and keeps the level filter at TRACE instead of falling back to the thread
    // default.
    keepalive();
    let captured = CapturedLogs::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
        .with_ansi(false)
        .event_format(super::HumanFormat::default())
        .with_max_level(level)
        .with_writer(move || writer.clone())
        .finish();
    (tracing::Dispatch::new(subscriber), captured)
}
