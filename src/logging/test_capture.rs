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

pub(crate) fn subscriber(level: tracing::Level) -> (tracing::Dispatch, CapturedLogs) {
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
