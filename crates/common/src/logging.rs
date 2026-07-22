//! Shared tracing/logging initialization for all Browser Hive binaries.
//!
//! Every binary (worker, coordinator, and downstream production workers) calls
//! [`init_logging`] instead of building its own subscriber, so the log format and
//! filtering behaviour stay identical across the fleet.

use anyhow::Result;
use tracing_subscriber::EnvFilter;

/// Initialize the global tracing subscriber.
///
/// **Format** is selected by the `LOG_FORMAT` environment variable:
/// - unset or `json` → structured JSON, one object per line. This is the default so
///   production pods emit Loki-friendly logs without any deployment change.
/// - `pretty` / `text` / `plain` / `human` → the human-readable format for local dev.
///
/// **Level** comes from `RUST_LOG` (standard `EnvFilter` syntax), defaulting to `info`.
///
/// ## Why JSON is shaped this way
///
/// Per-request context (ray_id, url, wait_selector, …) is attached to a `tracing` span,
/// not baked into each message. With `with_current_span(true)` those span fields are
/// emitted under a `span` object, so the message line stays clean while Loki's `| json`
/// parser exposes them as `span_ray_id`, `span_url`, `span_country_code`, … — filterable
/// per field and shown in the expanded log view. `flatten_event(true)` keeps the event's
/// own fields (including `message`) at the top level.
pub fn init_logging() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if use_pretty_format() {
        let subscriber = tracing_subscriber::fmt().with_env_filter(filter).finish();
        tracing::subscriber::set_global_default(subscriber)?;
    } else {
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_env_filter(filter)
            .finish();
        tracing::subscriber::set_global_default(subscriber)?;
    }

    Ok(())
}

/// `true` when the human-readable format is requested. JSON is the default.
fn use_pretty_format() -> bool {
    match std::env::var("LOG_FORMAT") {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "pretty" | "text" | "plain" | "human"
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// Collects subscriber output into a shared buffer so a test can assert on it.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The JSON contract Loki relies on: per-request context lives under a `span` object
    /// (so `| json` exposes it as `span_ray_id`, `span_url`, …) while the event's own
    /// message stays at the top level, and `Empty` fields recorded later still appear.
    #[test]
    fn json_puts_request_context_under_span_object() {
        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_writer(buf.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "scrape_page",
                ray_id = "ray-123",
                url = "http://example/x",
                wait_selector = tracing::field::Empty,
            );
            span.record("wait_selector", "div.foo");
            let _enter = span.enter();
            tracing::info!("hello");
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("\"message\":\"hello\""),
            "message flattened: {out}"
        );
        assert!(out.contains("\"span\":{"), "span object present: {out}");
        assert!(
            out.contains("\"ray_id\":\"ray-123\""),
            "ray_id under span: {out}"
        );
        assert!(
            out.contains("\"wait_selector\":\"div.foo\""),
            "field recorded after creation is present: {out}"
        );
    }
}
