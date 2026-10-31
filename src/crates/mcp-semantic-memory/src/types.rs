//! The data the service passes around. These types know nothing about HTTP or
//! SQL.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the facades put in their typed errors when a library under them
/// failed.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A list of numbers that stands for a text. Every vector is compared with
/// every other one, so they are all the same kind of thing.
pub type Vector = Vec<f32>;

/// A number the database hands out. Each kind is its own type, so the compiler
/// refuses to use one where another is meant.
macro_rules! database_number {
  ($name:ident, $doc:literal) => {
    #[doc = $doc]
    #[derive(
      Debug,
      Clone,
      Copy,
      PartialEq,
      Eq,
      PartialOrd,
      Ord,
      Hash,
      Serialize,
      Deserialize,
    )]
    #[serde(transparent)]
    pub struct $name(i64);

    impl $name {
      /// Wraps a number read from the database.
      pub const fn new(number: i64) -> Self {
        Self(number)
      }

      /// The number, for writing it to the database.
      pub const fn get(self) -> i64 {
        self.0
      }
    }

    impl fmt::Display for $name {
      fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
      }
    }
  };
}

database_number!(MemoryId, "The id of a memory.");
database_number!(EventId, "The id of an event.");
database_number!(
  Version,
  "How many writes a memory has had. The write path compares it to notice a \
   memory that changed between its two calls."
);

impl Version {
  /// The version after one more write.
  pub const fn next(self) -> Self {
    Self(self.0.saturating_add(1))
  }
}

/// What makes two vectors comparable: the model that made them, how long they
/// are, and the distance between them, which is always cosine.
///
/// ```
/// use mcp_semantic_memory::types::Fingerprint;
///
/// let fingerprint = Fingerprint {
///   model: "nomic".to_owned(),
///   dimension: 768,
/// };
/// assert_eq!(fingerprint.to_string(), "nomic/768/cosine");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
  /// The label of the embedding model.
  pub model: String,
  /// How many numbers a vector has.
  pub dimension: usize,
}

impl fmt::Display for Fingerprint {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}/{}/cosine", self.model, self.dimension)
  }
}

/// Something the model told the memory, before it is stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEvent {
  /// What happened, in the model's words.
  pub content: String,
  /// Free form. The model puts the session or occasion here.
  #[serde(default)]
  pub metadata: Value,
}

/// A stored event. Never changed after it is written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
  /// The id of the event.
  pub id: EventId,
  /// The memory the event is attached to.
  pub memory_id: MemoryId,
  /// What happened, in the model's words.
  pub content: String,
  /// Free form, as the model gave it.
  pub metadata: Value,
  /// The memory text this event produced.
  pub memory_text: String,
  /// When the event was written.
  pub created_at: DateTime<Utc>,
}

/// The flat, searchable layer: one text that events are attached to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
  /// The id of the memory.
  pub id: MemoryId,
  /// The text that is searched.
  pub content: String,
  /// The hash of `content`. No two memories have the same one.
  pub content_hash: String,
  /// How many events are attached.
  pub event_count: i64,
  /// When the latest event was written.
  pub last_event_at: DateTime<Utc>,
  /// How close the memory is to the current interests, if there are any.
  pub interest_score: Option<f32>,
  /// Set when consolidation forgets the memory. Cleared when an event
  /// attaches to it again.
  pub forgotten_at: Option<DateTime<Utc>>,
  /// Counts writes. The write path uses it to notice a memory that changed
  /// between its two calls.
  pub version: Version,
  /// When the memory was made.
  pub created_at: DateTime<Utc>,
  /// When the memory last changed.
  pub updated_at: DateTime<Utc>,
}

/// A memory near some vector, and how far away it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
  /// The memory that was found.
  pub memory: Memory,
  /// The cosine distance from the vector that was searched for.
  pub distance: f32,
}

/// What the model decides on the second call of the write path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
  /// The event starts a memory of its own.
  New,
  /// The event belongs to this memory, which now reads `memory_text`.
  Attach {
    /// The memory the event belongs to.
    memory_id: MemoryId,
    /// The version the candidate had.
    version: Version,
    /// The new text of the memory.
    memory_text: String,
  },
  /// The event only repeats this memory, whose text stays.
  Repeat {
    /// The memory the event repeats.
    memory_id: MemoryId,
    /// The version the candidate had.
    version: Version,
  },
}

/// The answer to `memory_store`.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreOutcome {
  /// The event is written and attached to this memory.
  Written {
    /// The memory the event is attached to.
    memory_id: MemoryId,
  },
  /// Nothing is written. The model has to decide what the event is.
  NeedsDecision {
    /// The memories the event may belong to.
    candidates: Vec<Candidate>,
  },
}

/// A memory fetched by id, with its events if they were asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recall {
  /// The memory that was asked for.
  pub memory: Memory,
  /// Newest first.
  pub events: Option<Vec<Event>>,
}

/// A memory found by a search, and how well it ranked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
  /// The memory that was found.
  pub memory: Memory,
  /// Higher is better.
  pub score: f32,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_number_shows_and_serializes_as_the_plain_number() {
    assert_eq!(MemoryId::new(7).to_string(), "7");
    assert_eq!(serde_json::to_string(&EventId::new(7)).unwrap(), "7");
    assert_eq!(
      serde_json::from_str::<Version>("3").unwrap(),
      Version::new(3)
    );
  }

  #[test]
  fn the_next_version_is_one_more() {
    assert_eq!(Version::new(1).next(), Version::new(2));
    assert_eq!(MemoryId::new(5).get(), 5);
  }
}
