use crate::engine::ENGINE;
use tracing_subscriber::layer::Context;

pub type LogCallback = unsafe extern "C" fn(u32, *const u8, usize);

pub const LOG_ERROR: u32 = 3;
pub const LOG_WARN: u32 = 2;
pub const LOG_INFO: u32 = 1;
pub const LOG_DEBUG: u32 = 0;

pub fn dispatch_log(level: u32, message: &str) {
    if let Ok(callback_lock) = ENGINE.log_callback.try_lock() {
        if let Some(cb) = *callback_lock {
            // SAFETY: cb is a valid function pointer registered by the caller; message is alive for the call duration.
            unsafe {
                cb(level, message.as_ptr(), message.len());
            }
        }
    }
}

pub struct ExternalLogLayer;

impl<S> tracing_subscriber::Layer<S> for ExternalLogLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let message = format_event(event);

        let level = match *event.metadata().level() {
            tracing::Level::ERROR => LOG_ERROR,
            tracing::Level::WARN => LOG_WARN,
            tracing::Level::INFO => LOG_INFO,
            _ => LOG_DEBUG,
        };

        dispatch_log(level, &message);
    }
}

// Separate budgets keep the message readable even when tracing records fields
// first. Formatting writes directly into bounded UTF-8 buffers, with no temporary
// unbounded Debug strings. Disabled events never enter on_event.
const MAX_LOG_BYTES: usize = 4096;

struct LogBuffer {
    text: String,
    limit: usize,
}

impl LogBuffer {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
        }
    }
}

impl std::fmt::Write for LogBuffer {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let remaining = self.limit - self.text.len();
        if value.len() <= remaining {
            self.text.push_str(value);
            return Ok(());
        }
        let mut end = remaining;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        // Reserve space for an explicit truncation marker, preserving UTF-8.
        let mut end = self.text.len().min(self.limit - 3);
        while !self.text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.truncate(end);
        self.text.push_str("...");
        Err(std::fmt::Error)
    }
}

struct LogVisitor {
    message: LogBuffer,
    fields: LogBuffer,
}

impl tracing::field::Visit for LogVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let separator = if self.fields.text.is_empty() { "" } else { " " };
            let _ = write!(self.fields, "{separator}{}={value:?}", field.name());
        }
    }
}

fn format_event(event: &tracing::Event<'_>) -> String {
    let mut visitor = LogVisitor {
        message: LogBuffer::new(MAX_LOG_BYTES / 2),
        fields: LogBuffer::new(MAX_LOG_BYTES / 2 - 1),
    };
    event.record(&mut visitor);
    let mut message = visitor.message.text;
    // Preserve the previous callback's message presentation for string values.
    if message.starts_with('"') && message.ends_with('"') && message.len() >= 2 {
        message.remove(0);
        message.pop();
    }
    if !visitor.fields.text.is_empty() {
        if !message.is_empty() {
            message.push(' ');
        }
        message.push_str(&visitor.fields.text);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;

    struct Capture(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            self.0.lock().unwrap().push(format_event(event));
        }
    }

    #[test]
    fn forwarded_lines_include_structured_fields_and_preserve_message() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(lines.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(cid = "block", index = 7, found = true, room = %"main", error = ?Some("busy"), "chunk received");
            tracing::info!("plain message");
        });
        let lines = lines.lock().unwrap();
        assert_eq!(
            lines[0],
            "chunk received cid=\"block\" index=7 found=true room=main error=Some(\"busy\")"
        );
        assert_eq!(lines[1], "plain message");
    }

    #[test]
    fn forwarded_lines_bound_large_fields_and_messages_on_utf8_boundaries() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(lines.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let huge = "あ".repeat(MAX_LOG_BYTES);
            tracing::debug!(cid = %huge, other = ?huge, "response {}", huge);
        });
        let lines = lines.lock().unwrap();
        assert!(lines[0].len() <= MAX_LOG_BYTES);
        assert!(lines[0].contains("..."));
        assert!(lines[0].contains("cid="));
    }

    #[test]
    fn disabled_events_do_not_format_values() {
        struct MustNotFormat;
        impl std::fmt::Debug for MustNotFormat {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("disabled field was formatted");
            }
        }
        let subscriber = tracing_subscriber::registry()
            .with(ExternalLogLayer.with_filter(tracing_subscriber::filter::LevelFilter::INFO));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(value = ?MustNotFormat, "disabled");
        });
    }
}
