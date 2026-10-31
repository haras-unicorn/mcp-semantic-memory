//! In-memory stand-ins for the three facades, for tests.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::{Mutex, MutexGuard};

use crate::embedder::{EmbedError, EmbedKind, Embedder};
use crate::reranker::{RerankError, Reranker};
use crate::store::{
  Consolidated, Consolidation, Embedded, Forgotten, Health, MemoryText, Store,
  StoreError, Swapped, Write, WriteOutcome,
};
use crate::types::{
  Candidate, Event, EventId, Fingerprint, Memory, MemoryId, NewEvent, Vector,
  Version,
};
use crate::vector::cosine_distance;

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
  text
    .split(|c: char| !c.is_alphanumeric())
    .filter(|word| !word.is_empty())
    .map(str::to_lowercase)
}

/// The axis a word without a vector pushes along: FNV-1a of its spelling.
fn axis(word: &str, dimension: usize) -> usize {
  let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
  for byte in word.bytes() {
    hash ^= u64::from(byte);
    hash = hash.wrapping_mul(0x0100_0000_01b3);
  }
  let dimension = u64::try_from(dimension).unwrap_or(1);
  usize::try_from(hash.checked_rem(dimension).unwrap_or(0)).unwrap_or(0)
}

fn count(length: usize) -> i64 {
  i64::try_from(length).unwrap_or(i64::MAX)
}

/// Turns a text into the sum of the vectors given for its words, so a test can
/// say exactly which texts are close. A word without a vector adds a small
/// push along an axis picked from its spelling.
#[derive(Debug, Clone)]
pub struct FakeEmbedder {
  fingerprint: Fingerprint,
  words: HashMap<String, Vector>,
}

impl FakeEmbedder {
  pub fn new(dimension: usize) -> Self {
    assert!(dimension > 0, "a vector needs at least one dimension");
    Self {
      fingerprint: Fingerprint {
        model: "fake".to_owned(),
        dimension,
      },
      words: HashMap::new(),
    }
  }

  pub fn with_word(mut self, word: &str, vector: Vector) -> Self {
    assert_eq!(vector.len(), self.fingerprint.dimension);
    self.words.insert(word.to_lowercase(), vector);
    self
  }

  fn vectorize(&self, text: &str) -> Vector {
    let dimension = self.fingerprint.dimension;
    let mut sum = vec![0.0_f32; dimension];
    for word in words(text) {
      match self.words.get(&word) {
        Some(vector) => {
          for (total, part) in sum.iter_mut().zip(vector) {
            *total += part;
          }
        }
        None => {
          if let Some(total) = sum.get_mut(axis(&word, dimension)) {
            *total += 0.01;
          }
        }
      }
    }
    sum
  }
}

#[async_trait]
impl Embedder for FakeEmbedder {
  async fn embed(
    &self,
    _kind: EmbedKind,
    texts: &[String],
  ) -> Result<Vec<Vector>, EmbedError> {
    Ok(texts.iter().map(|text| self.vectorize(text)).collect())
  }

  fn fingerprint(&self) -> &Fingerprint {
    &self.fingerprint
  }
}

/// Scores a document by how many words of the query it contains.
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeReranker;

#[async_trait]
impl Reranker for FakeReranker {
  async fn rerank(
    &self,
    query: &str,
    documents: &[String],
  ) -> Result<Vec<f32>, RerankError> {
    let wanted: Vec<String> = words(query).collect();
    Ok(
      documents
        .iter()
        .map(|document| {
          let have: Vec<String> = words(document).collect();
          let shared = wanted.iter().filter(|word| have.contains(word)).count();
          f32::from(u16::try_from(shared).unwrap_or(u16::MAX))
        })
        .collect(),
    )
  }
}

#[derive(Debug, Clone)]
struct Row {
  memory: Memory,
  vector: Vector,
}

#[derive(Debug, Default)]
struct State {
  rows: Vec<Row>,
  events: Vec<Event>,
  interests: Vec<Vector>,
  run_at: Option<DateTime<Utc>>,
  consolidations: usize,
  to_reembed: u64,
  next: Option<HashMap<MemoryId, Vector>>,
}

impl State {
  fn active(&self) -> impl Iterator<Item = &Row> {
    self
      .rows
      .iter()
      .filter(|row| row.memory.forgotten_at.is_none())
  }

  /// The same arithmetic as the real stores: the newest event against the
  /// latest run, or the oldest active memory when there was none.
  fn since_consolidation(&self) -> Option<Duration> {
    let oldest = self.active().map(|row| row.memory.created_at).min()?;
    let newest = self.rows.iter().map(|row| row.memory.last_event_at).max()?;
    let behind = newest.signed_duration_since(self.run_at.unwrap_or(oldest));
    Some(behind.to_std().unwrap_or(Duration::ZERO))
  }

  fn row(&self, id: MemoryId) -> Option<&Row> {
    self.rows.iter().find(|row| row.memory.id == id)
  }

  fn row_mut(&mut self, id: MemoryId) -> Option<&mut Row> {
    self.rows.iter_mut().find(|row| row.memory.id == id)
  }

  /// The memory that already has this text, other than `except`.
  fn holder_of(
    &self,
    hash: &str,
    except: Option<MemoryId>,
  ) -> Option<MemoryId> {
    self
      .rows
      .iter()
      .find(|row| {
        row.memory.content_hash == hash && Some(row.memory.id) != except
      })
      .map(|row| row.memory.id)
  }

  fn add_event(
    &mut self,
    memory_id: MemoryId,
    event: NewEvent,
    memory_text: String,
    now: DateTime<Utc>,
  ) {
    self.events.push(Event {
      id: EventId::new(count(self.events.len()).saturating_add(1)),
      memory_id,
      content: event.content,
      metadata: event.metadata,
      memory_text,
      created_at: now,
    });
  }
}

fn earlier(time: DateTime<Utc>, ago: TimeDelta) -> DateTime<Utc> {
  time.checked_sub_signed(ago).unwrap_or(time)
}

fn touch(row: &mut Row, now: DateTime<Utc>) {
  row.memory.event_count = row.memory.event_count.saturating_add(1);
  row.memory.last_event_at = now;
  row.memory.forgotten_at = None;
  row.memory.version = row.memory.version.next();
  row.memory.updated_at = now;
}

/// A store that keeps everything in a `Vec`. It follows the same rules as a
/// real one: nothing is deleted, writes check versions and texts.
#[derive(Debug, Default)]
pub struct FakeStore {
  state: Mutex<State>,
}

impl FakeStore {
  pub fn new() -> Self {
    Self::default()
  }

  pub async fn set_interests(&self, interests: Vec<Vector>) {
    self.lock().await.interests = interests;
  }

  /// Moves everything that happened so far back in time by `ago`, the way a
  /// long quiet spell would look from the next event.
  pub async fn age_activity(&self, ago: TimeDelta) {
    let mut state = self.lock().await;
    for row in &mut state.rows {
      row.memory.created_at = earlier(row.memory.created_at, ago);
      row.memory.last_event_at = earlier(row.memory.last_event_at, ago);
      row.memory.updated_at = earlier(row.memory.updated_at, ago);
    }
    for event in &mut state.events {
      event.created_at = earlier(event.created_at, ago);
    }
    if let Some(run_at) = &mut state.run_at {
      *run_at = earlier(*run_at, ago);
    }
  }

  /// How many times a consolidation was written.
  pub async fn consolidations(&self) -> usize {
    self.lock().await.consolidations
  }

  /// Makes `health` say that this many memories need a reembed.
  pub async fn set_to_reembed(&self, count: u64) {
    self.lock().await.to_reembed = count;
  }

  /// A new memory of its own, written as the model would write it.
  pub async fn add(&self, text: &str, vector: Vector) -> MemoryId {
    let write = Write::New {
      event: NewEvent {
        content: text.to_owned(),
        metadata: serde_json::Value::Null,
      },
      memory: MemoryText {
        text: text.to_owned(),
        hash: text.to_owned(),
        vector,
        interest_score: None,
      },
    };
    match self.write(write).await {
      Ok(WriteOutcome::Written(id)) => id,
      other => panic!("expected a write, got {other:?}"),
    }
  }

  /// Forgets a memory the way consolidation would.
  pub async fn forget(&self, id: MemoryId) {
    if let Some(row) = self.lock().await.row_mut(id) {
      row.memory.forgotten_at = Some(Utc::now());
    }
  }

  async fn lock(&self) -> MutexGuard<'_, State> {
    self.state.lock().await
  }
}

#[async_trait]
impl Store for FakeStore {
  async fn migrate(&self) -> Result<(), StoreError> {
    Ok(())
  }

  async fn nearest(
    &self,
    vector: &[f32],
    limit: usize,
    forgotten: Forgotten,
  ) -> Result<Vec<Candidate>, StoreError> {
    let state = self.lock().await;
    let mut found: Vec<Candidate> = state
      .rows
      .iter()
      .filter(|row| {
        forgotten == Forgotten::Include || row.memory.forgotten_at.is_none()
      })
      .map(|row| Candidate {
        memory: row.memory.clone(),
        distance: cosine_distance(vector, &row.vector),
      })
      .collect();
    found.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    found.truncate(limit);
    Ok(found)
  }

  async fn keyword(
    &self,
    query: &str,
    limit: usize,
  ) -> Result<Vec<Memory>, StoreError> {
    let state = self.lock().await;
    let wanted: Vec<String> = words(query).collect();
    let mut found: Vec<(usize, Memory)> = state
      .rows
      .iter()
      .filter(|row| row.memory.forgotten_at.is_none())
      .filter_map(|row| {
        let have: Vec<String> = words(&row.memory.content).collect();
        let hits = wanted.iter().filter(|word| have.contains(word)).count();
        (hits > 0).then(|| (hits, row.memory.clone()))
      })
      .collect();
    found.sort_by_key(|(hits, _)| std::cmp::Reverse(*hits));
    found.truncate(limit);
    Ok(found.into_iter().map(|(_, memory)| memory).collect())
  }

  async fn write(&self, write: Write) -> Result<WriteOutcome, StoreError> {
    let mut state = self.lock().await;
    let now = Utc::now();
    match write {
      Write::New { event, memory } => {
        if let Some(id) = state.holder_of(&memory.hash, None) {
          return Ok(WriteOutcome::Duplicate(id));
        }
        let id = MemoryId::new(count(state.rows.len()).saturating_add(1));
        state.add_event(id, event, memory.text.clone(), now);
        state.rows.push(Row {
          memory: Memory {
            id,
            content: memory.text,
            content_hash: memory.hash,
            event_count: 1,
            last_event_at: now,
            interest_score: memory.interest_score,
            forgotten_at: None,
            version: Version::new(1),
            created_at: now,
            updated_at: now,
          },
          vector: memory.vector,
        });
        Ok(WriteOutcome::Written(id))
      }
      Write::Attach {
        memory_id,
        version,
        event,
        memory,
      } => {
        let Some(current) = state.row(memory_id).map(|r| r.memory.version)
        else {
          return Err(StoreError::MissingMemory(memory_id));
        };
        if current != version {
          return Ok(WriteOutcome::Conflict);
        }
        if let Some(id) = state.holder_of(&memory.hash, Some(memory_id)) {
          return Ok(WriteOutcome::Duplicate(id));
        }
        state.add_event(memory_id, event, memory.text.clone(), now);
        if let Some(row) = state.row_mut(memory_id) {
          row.memory.content = memory.text;
          row.memory.content_hash = memory.hash;
          row.memory.interest_score = memory.interest_score;
          row.vector = memory.vector;
          touch(row, now);
        }
        Ok(WriteOutcome::Written(memory_id))
      }
      Write::Repeat {
        memory_id,
        version,
        event,
      } => {
        let Some((current, text)) = state
          .row(memory_id)
          .map(|r| (r.memory.version, r.memory.content.clone()))
        else {
          return Err(StoreError::MissingMemory(memory_id));
        };
        if current != version {
          return Ok(WriteOutcome::Conflict);
        }
        state.add_event(memory_id, event, text, now);
        if let Some(row) = state.row_mut(memory_id) {
          touch(row, now);
        }
        Ok(WriteOutcome::Written(memory_id))
      }
    }
  }

  async fn memory(&self, id: MemoryId) -> Result<Option<Memory>, StoreError> {
    Ok(self.lock().await.row(id).map(|row| row.memory.clone()))
  }

  async fn events(
    &self,
    memory_id: MemoryId,
    limit: usize,
  ) -> Result<Vec<Event>, StoreError> {
    let state = self.lock().await;
    Ok(
      state
        .events
        .iter()
        .rev()
        .filter(|event| event.memory_id == memory_id)
        .take(limit)
        .cloned()
        .collect(),
    )
  }

  async fn interests(&self) -> Result<Vec<Vector>, StoreError> {
    Ok(self.lock().await.interests.clone())
  }

  async fn health(&self) -> Result<Health, StoreError> {
    let state = self.lock().await;
    Ok(Health {
      memories_to_reembed: state.to_reembed,
      interests_to_rebuild: 0,
      since_consolidation: state.since_consolidation(),
    })
  }

  async fn sample(&self, limit: usize) -> Result<Vec<Embedded>, StoreError> {
    let state = self.lock().await;
    let mut rows: Vec<&Row> = state.active().collect();
    rows.sort_by(|a, b| {
      b.memory
        .event_count
        .cmp(&a.memory.event_count)
        .then(b.memory.last_event_at.cmp(&a.memory.last_event_at))
        .then(a.memory.id.cmp(&b.memory.id))
    });
    Ok(rows.into_iter().take(limit).map(embedded).collect())
  }

  async fn active(
    &self,
    after: Option<MemoryId>,
    limit: usize,
  ) -> Result<Vec<Embedded>, StoreError> {
    let state = self.lock().await;
    let mut rows: Vec<&Row> = state
      .active()
      .filter(|row| after.is_none_or(|after| row.memory.id > after))
      .collect();
    rows.sort_by_key(|row| row.memory.id);
    Ok(rows.into_iter().take(limit).map(embedded).collect())
  }

  async fn consolidate(
    &self,
    consolidation: Consolidation,
    only_if_needed: Option<Duration>,
  ) -> Result<Consolidated, StoreError> {
    let mut state = self.lock().await;
    if let Some(max_age) = only_if_needed {
      let health = Health {
        memories_to_reembed: 0,
        interests_to_rebuild: 0,
        since_consolidation: state.since_consolidation(),
      };
      if !health.needs_consolidation(max_age) {
        return Ok(Consolidated::NotStale);
      }
    }

    let now = Utc::now();
    state.run_at = Some(now);
    state.interests = consolidation
      .interests
      .into_iter()
      .map(|interest| interest.centre)
      .collect();
    for verdict in consolidation.verdicts {
      if let Some(row) = state.row_mut(verdict.memory_id)
        && row.memory.version == verdict.version
      {
        row.memory.interest_score = Some(verdict.interest_score);
        if verdict.forget {
          row.memory.forgotten_at = Some(now);
        }
      }
    }
    state.consolidations = state.consolidations.saturating_add(1);
    Ok(Consolidated::Written)
  }

  async fn begin_reembed(&self) -> Result<(), StoreError> {
    self.lock().await.next.get_or_insert_with(HashMap::new);
    Ok(())
  }

  async fn unembedded(&self, limit: usize) -> Result<Vec<Memory>, StoreError> {
    let state = self.lock().await;
    let done = state.next.as_ref();
    Ok(
      state
        .rows
        .iter()
        .filter(|row| {
          done.is_none_or(|done| !done.contains_key(&row.memory.id))
        })
        .take(limit)
        .map(|row| row.memory.clone())
        .collect(),
    )
  }

  async fn put_reembedded(
    &self,
    vectors: Vec<(MemoryId, Vector)>,
  ) -> Result<(), StoreError> {
    let mut state = self.lock().await;
    state.next.get_or_insert_with(HashMap::new).extend(vectors);
    Ok(())
  }

  async fn swap_vectors(&self) -> Result<Swapped, StoreError> {
    let mut state = self.lock().await;
    let Some(next) = state.next.take() else {
      return Ok(Swapped::Missing);
    };
    if state
      .rows
      .iter()
      .any(|row| !next.contains_key(&row.memory.id))
    {
      state.next = Some(next);
      return Ok(Swapped::Missing);
    }
    for row in &mut state.rows {
      if let Some(vector) = next.get(&row.memory.id) {
        row.vector.clone_from(vector);
      }
    }
    state.interests.clear();
    state.to_reembed = 0;
    Ok(Swapped::Done)
  }
}

fn embedded(row: &Row) -> Embedded {
  Embedded {
    memory: row.memory.clone(),
    vector: row.vector.clone(),
  }
}

#[cfg(test)]
mod tests {
  use serde_json::Value;

  use super::*;
  use crate::store::MemoryText;

  fn event(content: &str) -> NewEvent {
    NewEvent {
      content: content.to_owned(),
      metadata: Value::Null,
    }
  }

  fn text(text: &str, hash: &str, vector: Vector) -> MemoryText {
    MemoryText {
      text: text.to_owned(),
      hash: hash.to_owned(),
      vector,
      interest_score: None,
    }
  }

  async fn new_memory(
    store: &FakeStore,
    content: &str,
    vector: Vector,
  ) -> MemoryId {
    match store
      .write(Write::New {
        event: event(content),
        memory: text(content, content, vector),
      })
      .await
      .unwrap()
    {
      WriteOutcome::Written(id) => id,
      other => panic!("expected a write, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn embedder_puts_texts_with_shared_words_close() {
    let embedder = FakeEmbedder::new(3)
      .with_word("apple", vec![1.0, 0.0, 0.0])
      .with_word("pie", vec![0.0, 1.0, 0.0])
      .with_word("tart", vec![0.0, 0.0, 1.0]);
    let texts = ["apple pie", "apple", "tart"].map(str::to_owned);
    let vectors = embedder.embed(EmbedKind::Document, &texts).await.unwrap();
    assert!(
      cosine_distance(&vectors[0], &vectors[1])
        < cosine_distance(&vectors[0], &vectors[2])
    );
  }

  #[tokio::test]
  async fn nearest_returns_the_closest_memory_first() {
    let store = FakeStore::new();
    new_memory(&store, "a", vec![1.0, 0.0]).await;
    new_memory(&store, "b", vec![0.0, 1.0]).await;
    let found = store
      .nearest(&[0.9, 0.1], 2, Forgotten::Skip)
      .await
      .unwrap();
    assert_eq!(found[0].memory.content, "a");
    assert_eq!(found.len(), 2);
  }

  #[tokio::test]
  async fn a_stale_version_is_a_conflict_and_writes_nothing() {
    let store = FakeStore::new();
    let id = new_memory(&store, "a", vec![1.0, 0.0]).await;
    let outcome = store
      .write(Write::Repeat {
        memory_id: id,
        version: Version::new(99),
        event: event("again"),
      })
      .await
      .unwrap();
    assert_eq!(outcome, WriteOutcome::Conflict);
    assert_eq!(store.events(id, 10).await.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn attaching_to_a_forgotten_memory_brings_it_back() {
    let store = FakeStore::new();
    let id = new_memory(&store, "a", vec![1.0, 0.0]).await;
    store.forget(id).await;
    let active = store.nearest(&[1.0, 0.0], 1, Forgotten::Skip).await;
    assert!(active.unwrap().is_empty());
    let all = store.nearest(&[1.0, 0.0], 1, Forgotten::Include).await;
    assert_eq!(all.unwrap().len(), 1);

    let outcome = store
      .write(Write::Attach {
        memory_id: id,
        version: Version::new(1),
        event: event("a, again"),
        memory: text("a, again", "a, again", vec![1.0, 0.0]),
      })
      .await
      .unwrap();
    assert_eq!(outcome, WriteOutcome::Written(id));
    let memory = store.memory(id).await.unwrap().unwrap();
    assert!(memory.forgotten_at.is_none());
    assert_eq!(memory.event_count, 2);
    assert_eq!(memory.version, Version::new(2));
  }

  #[tokio::test]
  async fn the_same_text_twice_is_a_duplicate() {
    let store = FakeStore::new();
    let id = new_memory(&store, "a", vec![1.0, 0.0]).await;
    let outcome = store
      .write(Write::New {
        event: event("a"),
        memory: text("a", "a", vec![1.0, 0.0]),
      })
      .await
      .unwrap();
    assert_eq!(outcome, WriteOutcome::Duplicate(id));
  }
}
