//! Logging: one JSON object per line on stderr (stdout belongs to the MCP
//! transport).
//!
//! The event's fields sit at the top level next to `message`, so a metrics
//! pipeline can read the fields and a person can `jq` and `grep` the message.
//! Write the message as a short sentence and put values worth filtering or
//! counting on in fields: `tracing::info!(memory_id, "stored a memory")`.

use anyhow::{Context, Result};
use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Logs at `info` unless `RUST_LOG` says otherwise.
pub fn init() -> Result<()> {
  let filter = EnvFilter::try_from_default_env()
    .unwrap_or_else(|_| EnvFilter::new("info"));
  tracing::subscriber::set_global_default(subscriber(std::io::stderr, filter))
    .context("could not set up logging")
}

fn subscriber<W>(writer: W, filter: EnvFilter) -> impl Subscriber + Send + Sync
where
  W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
  tracing_subscriber::fmt()
    .json()
    .flatten_event(true)
    .with_current_span(false)
    .with_span_list(false)
    .with_env_filter(filter)
    .with_writer(writer)
    .finish()
}

#[cfg(test)]
mod tests {
  use std::io;
  use std::sync::{Arc, Mutex};

  use serde_json::Value;

  use super::*;

  #[derive(Clone, Default)]
  struct Buffer(Arc<Mutex<Vec<u8>>>);

  impl io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
      self.0.lock().unwrap().extend_from_slice(bytes);
      Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  #[test]
  fn a_line_has_the_message_and_the_fields_at_the_top_level() {
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = subscriber(move || writer.clone(), EnvFilter::new("info"));

    tracing::subscriber::with_default(subscriber, || {
      tracing::info!(memory_id = 7, "stored a memory");
      tracing::debug!("not shown");
    });

    let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<Value> = output
      .lines()
      .map(|line| serde_json::from_str(line).unwrap())
      .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["message"], "stored a memory");
    assert_eq!(lines[0]["memory_id"], 7);
    assert_eq!(lines[0]["level"], "INFO");
  }

  #[test]
  fn init_sets_up_logging_once() {
    init().unwrap();
    assert!(init().is_err());
  }
}
