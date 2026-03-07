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
        let mut message = String::new();
        let mut visitor = LogVisitor {
            message: &mut message,
        };
        event.record(&mut visitor);

        let level = match *event.metadata().level() {
            tracing::Level::ERROR => LOG_ERROR,
            tracing::Level::WARN => LOG_WARN,
            tracing::Level::INFO => LOG_INFO,
            _ => LOG_DEBUG,
        };

        dispatch_log(level, &message);
    }
}

pub struct LogVisitor<'a> {
    pub message: &'a mut String,
}

impl<'a> tracing::field::Visit for LogVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let val = format!("{:?}", value);

            let trimmed = if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                &val[1..val.len() - 1]
            } else {
                &val
            };
            self.message.push_str(trimmed);
        }
    }
}
