//! In-memory ring buffer of recent log lines, wired via a `tracing`
//! `Layer`. The Debug modal renders a snapshot of this buffer so users can
//! inspect what the app has logged this session without opening a terminal.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fmt::Write;
use std::sync::Arc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

const CAPACITY: usize = 500;

#[derive(Clone, Debug)]
pub struct LogLine {
    pub level: Level,
    pub timestamp: time::OffsetDateTime,
    pub message: String,
}

#[derive(Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<LogLine>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY))),
        }
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.inner.lock().iter().cloned().collect()
    }

    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    fn push(&self, line: LogLine) {
        let mut q = self.inner.lock();
        if q.len() >= CAPACITY {
            q.pop_front();
        }
        q.push_back(line);
    }
}

pub struct LogBufferLayer {
    buf: LogBuffer,
}

impl LogBufferLayer {
    pub fn new(buf: LogBuffer) -> Self {
        Self { buf }
    }
}

impl<S: Subscriber> Layer<S> for LogBufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let now =
            time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        self.buf.push(LogLine {
            level: *event.metadata().level(),
            timestamp: now,
            message: visitor.0,
        });
    }
}

/// Collects the `message` field plus any structured key/value pairs into
/// a single string so the modal can render one line per event.
#[derive(Default)]
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            push_with_space(&mut self.0, value);
        } else {
            push_kv(&mut self.0, field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            push_with_space(&mut self.0, &format!("{value:?}"));
        } else {
            let sep = separator(&self.0);
            let _ = write!(&mut self.0, "{}{}={:?}", sep, field.name(), value);
        }
    }
}

fn push_with_space(buf: &mut String, value: &str) {
    if !buf.is_empty() {
        buf.push(' ');
    }
    buf.push_str(value);
}

fn push_kv(buf: &mut String, key: &str, value: &str) {
    let _ = write!(buf, "{}{}={}", separator(buf), key, value);
}

fn separator(buf: &str) -> &'static str {
    if buf.is_empty() {
        ""
    } else {
        " "
    }
}
