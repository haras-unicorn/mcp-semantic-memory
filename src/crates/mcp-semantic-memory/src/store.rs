//! The database facade. There is one implementation per dialect, each in a
//! crate of its own.

use std::time::Duration;

use async_trait::async_trait;
use auto_impl::auto_impl;
use thiserror::Error;

use crate::types::{
  BoxError, Candidate, Event, Memory, MemoryId, NewEvent, Vector, Version,
};

// NOTE: the stores turn the keywords of a text into a query of their own, so
// they get them from here and not from the ranking module they live in
pub use crate::ranking::keywords;

/// Why the database could not do what it was asked.
#[derive(Debug, Error)]
pub enum StoreError {
  /// The database or its driver failed.
  #[error("{what}: {source}")]
  Backend {
    /// What was being done.
    what: &'static str,
    /// What the driver reported.
    #[source]
    source: BoxError,
  },
  /// A write named a memory that does not exist.
  #[error("there is no memory {0}")]
  MissingMemory(MemoryId),
  /// The database holds something that cannot be read back.
  #[error("the database holds something unreadable: {0}")]
  Corrupt(String),
}

impl StoreError {
  /// For `map_err`: wraps what a driver reported with what was being done.
  pub fn backend<E>(what: &'static str) -> impl FnOnce(E) -> Self
  where
    E: std::error::Error + Send + Sync + 'static,
  {
    move |source| Self::Backend {
      what,
      source: Box::new(source),
    }
  }
}

impl From<sqlx::Error> for StoreError {
  fn from(error: sqlx::Error) -> Self {
    Self::backend("a query failed")(error)
  }
}

/// A memory's text with everything derived from it.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryText {
  /// The text.
  pub text: String,
  /// The hash of the text. No two memories have the same one.
  pub hash: String,
  /// The embedding of the text.
  pub vector: Vector,
  /// How close the text is to the current interests, if there are any.
  pub interest_score: Option<f32>,
}

/// One write. It is always an event, plus a new memory or a change to one.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
  /// The event starts a memory of its own.
  New {
    /// The event to write.
    event: NewEvent,
    /// The memory it starts.
    memory: MemoryText,
  },
  /// The event belongs to a memory, whose text changes.
  Attach {
    /// The memory the event belongs to.
    memory_id: MemoryId,
    /// The version the write expects the memory to have.
    version: Version,
    /// The event to write.
    event: NewEvent,
    /// The new text of the memory.
    memory: MemoryText,
  },
  /// The event repeats a memory, whose text stays.
  Repeat {
    /// The memory the event repeats.
    memory_id: MemoryId,
    /// The version the write expects the memory to have.
    version: Version,
    /// The event to write.
    event: NewEvent,
  },
}

/// What came of a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
  /// The event is written, and this is the memory it belongs to.
  Written(MemoryId),
  /// The memory's version is not the one the write expected. Nothing was
  /// written.
  Conflict,
  /// Another memory already has this text. Nothing was written.
  Duplicate(MemoryId),
}

/// Whether forgotten memories take part in a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forgotten {
  /// Only the active memories.
  Skip,
  /// The forgotten ones too, so an old memory can come back.
  Include,
}

/// A memory with its vector.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedded {
  /// The memory.
  pub memory: Memory,
  /// Its embedding.
  pub vector: Vector,
}

/// An interest that a consolidation found.
#[derive(Debug, Clone, PartialEq)]
pub struct NewInterest {
  /// The words that name the interest, separated by spaces.
  pub top_terms: String,
  /// The centre of the interest.
  pub centre: Vector,
}

/// What a consolidation decided about one memory. It was made from the
/// memory as it was at `version`, so a store leaves the memory alone when it
/// has changed since.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
  /// The memory.
  pub memory_id: MemoryId,
  /// The version the verdict is based on.
  pub version: Version,
  /// How close the memory is to the new interests.
  pub interest_score: f32,
  /// Whether the memory is forgotten.
  pub forget: bool,
}

/// The result of a consolidation, to be written in one go.
#[derive(Debug, Clone, PartialEq)]
pub struct Consolidation {
  /// The new interests, which replace the current ones.
  pub interests: Vec<NewInterest>,
  /// What became of the active memories.
  pub verdicts: Vec<Verdict>,
}

/// What came of asking a store to write a consolidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consolidated {
  /// The consolidation is written.
  Written,
  /// Another instance consolidated first, so the memory is no longer stale.
  /// Nothing was written.
  NotStale,
}

/// What came of asking a store to swap in the vectors of a reembed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swapped {
  /// The new vectors are the live ones, and the interest vectors are gone.
  Done,
  /// Some memory still has no new vector, for instance because it was written
  /// after the last page. Nothing was swapped.
  Missing,
}

/// What the startup checks need to know about the stored data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
  /// How many active memories have no vector, or one made by another
  /// embedding model.
  pub memories_to_reembed: u64,
  /// How many interests of the latest consolidation have no vector, or one
  /// made by another embedding model.
  pub interests_to_rebuild: u64,
  /// How far the newest event is ahead of the latest consolidation, never
  /// less than nothing. When there has been no consolidation, the oldest
  /// active memory stands in for it. Nothing when there are no active
  /// memories, since there is nothing to consolidate. Both times come from
  /// the database, and neither is the time now, so a quiet spell is not a
  /// reason to consolidate.
  pub since_consolidation: Option<Duration>,
}

/// Why the memory should not be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
  /// In words the model and the human can act on.
  pub message: String,
  /// Whether a consolidation puts everything right. It does not when there
  /// are memories to reembed, since clustering mixed vectors means nothing.
  pub consolidation_fixes_it: bool,
}

impl Health {
  /// Whether a consolidation would fix something: the latest one is further
  /// behind than `max_age`, or its interests lack current vectors.
  pub fn needs_consolidation(&self, max_age: Duration) -> bool {
    self.interests_to_rebuild > 0
      || self
        .since_consolidation
        .is_some_and(|since| since > max_age)
  }

  /// What is wrong, if anything. With `has_tool` the messages name the
  /// `memory_consolidate` tool as well as the command.
  pub fn problem(&self, max_age: Duration, has_tool: bool) -> Option<Problem> {
    let consolidate = if has_tool {
      "Call `memory_consolidate`, or run `consolidate`."
    } else {
      "Run `consolidate`."
    };
    let mut problems = Vec::new();
    if self.memories_to_reembed > 0 {
      problems.push(format!(
        "Active memories without a vector from the current embedding model: \
         {}. Run `reembed`.",
        self.memories_to_reembed
      ));
    }
    if self.interests_to_rebuild > 0 {
      problems.push(format!(
        "Interests without a vector from the current embedding model: {}. \
         {consolidate}",
        self.interests_to_rebuild
      ));
    }
    if let Some(since) = self.since_consolidation
      && since > max_age
    {
      problems.push(format!(
        "The last consolidation was {} behind the newest event and the \
         limit is {}. {consolidate}",
        seconds(since),
        seconds(max_age)
      ));
    }
    (!problems.is_empty()).then(|| Problem {
      message: problems.join(" "),
      consolidation_fixes_it: self.memories_to_reembed == 0,
    })
  }
}

/// A duration to the second, in words.
fn seconds(duration: Duration) -> String {
  humantime_serde::re::humantime::format_duration(Duration::from_secs(
    duration.as_secs(),
  ))
  .to_string()
}

/// Everything the service needs from a database. A write is one short
/// transaction. Nothing is ever deleted. A `Box<dyn Store>` is one too, so the
/// dialect in use can be picked while the program runs.
#[async_trait]
#[auto_impl(Box)]
pub trait Store: Send + Sync {
  /// Create or update the schema. A store is made with the embedding
  /// fingerprint, and its dimension sizes the vector tables.
  async fn migrate(&self) -> Result<(), StoreError>;

  /// The memories closest to `vector`, nearest first.
  async fn nearest(
    &self,
    vector: &[f32],
    limit: usize,
    forgotten: Forgotten,
  ) -> Result<Vec<Candidate>, StoreError>;

  /// The active memories that best match the words of `query`, best first.
  async fn keyword(
    &self,
    query: &str,
    limit: usize,
  ) -> Result<Vec<Memory>, StoreError>;

  /// Apply a write. An attach or a repeat clears `forgotten_at`, bumps the
  /// version and the event count, and sets the time of the last event.
  async fn write(&self, write: Write) -> Result<WriteOutcome, StoreError>;

  /// One memory by id, forgotten or not.
  async fn memory(&self, id: MemoryId) -> Result<Option<Memory>, StoreError>;

  /// The latest `limit` events of a memory, newest first.
  ///
  /// The newest events matter the most, so they are the ones kept when
  /// there are more than `limit`.
  async fn events(
    &self,
    memory_id: MemoryId,
    limit: usize,
  ) -> Result<Vec<Event>, StoreError>;

  /// The centroids of the latest interests. Empty before the first
  /// consolidation.
  async fn interests(&self) -> Result<Vec<Vector>, StoreError>;

  /// What the startup checks need. Vectors count as current when they carry
  /// the fingerprint this store was made with.
  async fn health(&self) -> Result<Health, StoreError>;

  /// Up to `limit` active memories, the most reinforced first: by event
  /// count, then by the time of the last event. A consolidation clusters
  /// these.
  async fn sample(&self, limit: usize) -> Result<Vec<Embedded>, StoreError>;

  /// A page of the active memories in the order of their ids, starting after
  /// `after`. Walking the pages visits every active memory once.
  async fn active(
    &self,
    after: Option<MemoryId>,
    limit: usize,
  ) -> Result<Vec<Embedded>, StoreError>;

  /// Writes a consolidation in one transaction, under a lock that only one
  /// instance can hold, so two of them cannot both write. The new interests
  /// become the latest ones and get one run time from the database's clock.
  /// Every verdict sets the interest score of its memory and, when it says
  /// so, forgets it, but only when the memory still has the version the
  /// verdict was made from.
  ///
  /// With `only_if_needed`, the lock is taken first and then the store works
  /// out `health` again. Unless `Health::needs_consolidation` says yes for
  /// that maximum age, nothing is written.
  async fn consolidate(
    &self,
    consolidation: Consolidation,
    only_if_needed: Option<Duration>,
  ) -> Result<Consolidated, StoreError>;

  /// Starts a reembed, or picks one up again: makes the table of new memory
  /// vectors, sized by this store's dimension, unless one made by this
  /// store's embedding model is already there with vectors in it. A table
  /// left by another model is dropped. The live vectors are not touched.
  async fn begin_reembed(&self) -> Result<(), StoreError>;

  /// Up to `limit` memories, forgotten ones too, that have no vector in the
  /// new table yet, in the order of their ids.
  async fn unembedded(&self, limit: usize) -> Result<Vec<Memory>, StoreError>;

  /// Puts vectors, made by this store's embedding model, in the new table.
  /// A memory that has one already gets it replaced.
  async fn put_reembedded(
    &self,
    vectors: Vec<(MemoryId, Vector)>,
  ) -> Result<(), StoreError>;

  /// Makes the new table the live one, and drops the interest vectors, which
  /// are in the old space. The interest rows stay, so a consolidation can
  /// make their vectors again. Nothing is swapped while some memory still has
  /// no new vector.
  async fn swap_vectors(&self) -> Result<Swapped, StoreError>;
}

#[cfg(test)]
mod tests {
  use super::*;

  const MAX_AGE: Duration = Duration::from_secs(60 * 60);

  fn healthy() -> Health {
    Health {
      memories_to_reembed: 0,
      interests_to_rebuild: 0,
      since_consolidation: Some(Duration::from_secs(60)),
    }
  }

  fn problem(health: Health) -> Problem {
    health.problem(MAX_AGE, false).unwrap()
  }

  #[test]
  fn a_healthy_store_has_no_problem() {
    assert_eq!(healthy().problem(MAX_AGE, false), None);
  }

  #[test]
  fn an_empty_store_has_no_problem() {
    let health = Health {
      since_consolidation: None,
      ..healthy()
    };
    assert_eq!(health.problem(MAX_AGE, false), None);
  }

  #[test]
  fn memories_from_another_model_ask_for_reembed() {
    let problem = problem(Health {
      memories_to_reembed: 3,
      ..healthy()
    });
    assert!(problem.message.contains("`reembed`"), "{problem:?}");
    assert!(problem.message.contains('3'), "{problem:?}");
    assert!(!problem.consolidation_fixes_it);
  }

  #[test]
  fn interests_from_another_model_ask_for_consolidate() {
    let problem = problem(Health {
      interests_to_rebuild: 2,
      ..healthy()
    });
    assert!(problem.message.contains("`consolidate`"), "{problem:?}");
    assert!(problem.consolidation_fixes_it);
  }

  #[test]
  fn an_old_consolidation_asks_for_consolidate() {
    let problem = problem(Health {
      since_consolidation: Some(MAX_AGE * 2),
      ..healthy()
    });
    assert!(problem.message.contains("2h"), "{problem:?}");
    assert!(problem.message.contains("`consolidate`"), "{problem:?}");
    assert!(
      !problem.message.contains("memory_consolidate"),
      "{problem:?}"
    );
    assert!(problem.consolidation_fixes_it);
  }

  #[test]
  fn with_the_tool_on_the_age_names_the_tool() {
    let health = Health {
      since_consolidation: Some(MAX_AGE * 2),
      ..healthy()
    };
    let problem = health.problem(MAX_AGE, true).unwrap();
    assert!(
      problem.message.contains("`memory_consolidate`"),
      "{problem:?}"
    );
    assert!(problem.consolidation_fixes_it);
  }

  #[test]
  fn memories_to_reembed_block_the_tool_too() {
    let health = Health {
      memories_to_reembed: 1,
      since_consolidation: Some(MAX_AGE * 2),
      ..healthy()
    };
    let problem = health.problem(MAX_AGE, true).unwrap();
    assert!(!problem.consolidation_fixes_it);
    assert!(problem.message.contains("`reembed`"), "{problem:?}");
  }

  #[test]
  fn a_consolidation_exactly_at_the_limit_is_fine() {
    let health = Health {
      since_consolidation: Some(MAX_AGE),
      ..healthy()
    };
    assert_eq!(health.problem(MAX_AGE, false), None);
  }

  #[test]
  fn every_problem_is_named() {
    let problem = problem(Health {
      memories_to_reembed: 1,
      interests_to_rebuild: 1,
      since_consolidation: Some(MAX_AGE * 2),
    });
    assert!(problem.message.contains("`reembed`"), "{problem:?}");
    assert!(problem.message.contains("Interests"), "{problem:?}");
    assert!(
      problem.message.contains("last consolidation"),
      "{problem:?}"
    );
  }
}
