//! The core service: the write path, search and recall. It composes the three
//! facades and knows nothing about HTTP, SQL or MCP.

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::config::{ConsolidationConfig, SearchConfig, WriteConfig};
use crate::consolidation::{self, Mode, Report};
use crate::embedder::{EmbedError, EmbedKind, Embedder};
use crate::ranking;
use crate::reranker::Reranker;
use crate::store::{
  Forgotten, MemoryText, Problem, Store, StoreError, Write, WriteOutcome,
};
use crate::types::{
  Candidate, Decision, MemoryId, NewEvent, Recall, SearchHit, StoreOutcome,
  Vector,
};
use crate::vector::cosine_distance;

/// Why the service could not do what it was asked.
#[derive(Debug, Error)]
pub enum ServiceError {
  /// The text is blank.
  #[error("{what} is empty")]
  Empty {
    /// What the text is, for the message.
    what: &'static str,
  },
  /// The text is longer than the configuration allows.
  #[error(
    "{what} is {length} characters long and the limit is {limit}: condense \
     it and try again"
  )]
  TooLong {
    /// What the text is, for the message.
    what: &'static str,
    /// How many characters the text has.
    length: usize,
    /// How many characters it may have.
    limit: usize,
  },
  /// The embedding server failed.
  #[error("could not embed the text: {0}")]
  Embed(#[from] EmbedError),
  /// The embedding server sent a different number of vectors than texts.
  #[error("the embedding server returned {got} vectors for one text")]
  VectorCount {
    /// How many vectors came back.
    got: usize,
  },
  /// The embedding server sent vectors of another length than configured.
  #[error(
    "the embedding server returned a vector of {got} numbers and \
     embedding.dimension is {expected}"
  )]
  VectorLength {
    /// How many numbers the vector has.
    got: usize,
    /// What `embedding.dimension` says.
    expected: usize,
  },
  /// The database failed.
  #[error(transparent)]
  Store(#[from] StoreError),
  /// The startup checks found something to fix before the memory can be
  /// used.
  #[error("the memory cannot be used yet. {0}")]
  NotReady(String),
}

/// The memory: writes events, searches and recalls, and consolidates when it
/// is told to.
pub struct Service<E, R, S> {
  embedder: E,
  reranker: R,
  store: S,
  write: WriteConfig,
  search: SearchConfig,
  consolidation: ConsolidationConfig,
}

impl<E: Embedder, R: Reranker, S: Store> Service<E, R, S> {
  /// Wires the three facades together with the settings of the write path and
  /// of search.
  pub fn new(
    embedder: E,
    reranker: R,
    store: S,
    write: WriteConfig,
    search: SearchConfig,
  ) -> Self {
    Self {
      embedder,
      reranker,
      store,
      write,
      search,
      consolidation: ConsolidationConfig::default(),
    }
  }

  /// Replaces the default settings of consolidation.
  #[must_use]
  pub fn with_consolidation(
    mut self,
    consolidation: ConsolidationConfig,
  ) -> Self {
    self.consolidation = consolidation;
    self
  }

  /// The settings of consolidation.
  pub fn consolidation(&self) -> &ConsolidationConfig {
    &self.consolidation
  }

  /// What is wrong with the stored data, if anything: the startup checks.
  pub async fn problem(&self) -> Result<Option<Problem>, ServiceError> {
    let health = self.store.health().await?;
    Ok(health.problem(self.consolidation.max_age, self.consolidation.has_tool))
  }

  /// The upkeep, for a deployment with no timer. It writes only if a
  /// consolidation is still needed once the store has its lock, so two
  /// instances asked at once consolidate once.
  pub async fn consolidate(&self) -> Result<Report, ServiceError> {
    let mode = Mode::Apply {
      only_if_needed: Some(self.consolidation.max_age),
    };
    Ok(
      consolidation::consolidate(
        &self.store,
        &self.consolidation,
        self.search.half_life,
        mode,
      )
      .await?,
    )
  }

  /// The write path. Without a decision this is the first call: the event
  /// is written when no memory is near it, and otherwise the near memories
  /// come back and nothing is written. With a decision it is the second
  /// call, and the event is written as the decision says. If a memory
  /// named by the decision changed since the first call, or the new text
  /// is already some other memory's text, the model is asked again.
  pub async fn store(
    &self,
    event: NewEvent,
    decision: Option<Decision>,
  ) -> Result<StoreOutcome, ServiceError> {
    self.check_text("the event", &event.content)?;
    if let Some(Decision::Attach { memory_text, .. }) = &decision {
      self.check_text("the memory text", memory_text)?;
    }

    let event_vector = self.embed(EmbedKind::Document, &event.content).await?;

    if decision.is_none() {
      let candidates = self.near(&event_vector).await?;
      if !candidates.is_empty() {
        return Ok(StoreOutcome::NeedsDecision { candidates });
      }
    }

    let write = match decision {
      None | Some(Decision::New) => Write::New {
        memory: self
          .memory_text(event.content.clone(), event_vector.clone())
          .await?,
        event,
      },
      Some(Decision::Attach {
        memory_id,
        version,
        memory_text,
      }) => {
        let vector = self.embed(EmbedKind::Document, &memory_text).await?;
        Write::Attach {
          memory_id,
          version,
          memory: self.memory_text(memory_text, vector).await?,
          event,
        }
      }
      Some(Decision::Repeat { memory_id, version }) => Write::Repeat {
        memory_id,
        version,
        event,
      },
    };

    let memory_vector = match &write {
      Write::New { memory, .. } | Write::Attach { memory, .. } => {
        Some(memory.vector.clone())
      }
      Write::Repeat { .. } => None,
    };

    match self.store.write(write).await? {
      WriteOutcome::Written(memory_id) => {
        Ok(StoreOutcome::Written { memory_id })
      }
      WriteOutcome::Conflict => Ok(StoreOutcome::NeedsDecision {
        candidates: self.near(&event_vector).await?,
      }),
      WriteOutcome::Duplicate(memory_id) => {
        let mut candidates = self.near(&event_vector).await?;
        if !candidates
          .iter()
          .any(|candidate| candidate.memory.id == memory_id)
          && let Some(memory) = self.store.memory(memory_id).await?
        {
          let distance = memory_vector
            .map_or(0.0, |vector| cosine_distance(&event_vector, &vector));
          candidates.push(Candidate { memory, distance });
        }
        Ok(StoreOutcome::NeedsDecision { candidates })
      }
    }
  }

  /// Hybrid search over the active memories. It writes nothing.
  pub async fn search(
    &self,
    query: &str,
    limit: Option<usize>,
  ) -> Result<Vec<SearchHit>, ServiceError> {
    if query.trim().is_empty() {
      return Err(ServiceError::Empty { what: "the query" });
    }

    let vector = self.embed(EmbedKind::Query, query).await?;
    let (nearest, keyword) = tokio::try_join!(
      self
        .store
        .nearest(&vector, self.search.candidates, Forgotten::Skip),
      self.store.keyword(query, self.search.candidates),
    )?;

    let mut hits = ranking::fuse(nearest, keyword, &self.search);
    self.rerank(query, &mut hits).await;
    hits.truncate(limit.unwrap_or(self.search.limit));
    Ok(hits)
  }

  /// One memory by id, with its events if asked. A forgotten memory is not
  /// there as far as the model can tell.
  pub async fn recall(
    &self,
    id: MemoryId,
    with_events: bool,
  ) -> Result<Option<Recall>, ServiceError> {
    let Some(memory) = self.store.memory(id).await? else {
      return Ok(None);
    };
    if memory.forgotten_at.is_some() {
      return Ok(None);
    }
    let events = if with_events {
      Some(self.store.events(id, self.search.max_events).await?)
    } else {
      None
    };
    Ok(Some(Recall { memory, events }))
  }

  /// The memories close enough to a vector to ask the model about. Forgotten
  /// ones are included, so an old memory can come back.
  async fn near(&self, vector: &[f32]) -> Result<Vec<Candidate>, StoreError> {
    let mut found = self
      .store
      .nearest(vector, self.write.max_candidates, Forgotten::Include)
      .await?;
    found.retain(|candidate| candidate.distance <= self.write.max_distance);
    Ok(found)
  }

  /// Reorders the best candidates by the reranker's scores. The reranker
  /// is an extra, so when it fails the fused order stands.
  async fn rerank(&self, query: &str, hits: &mut Vec<SearchHit>) {
    let count = hits.len().min(self.search.rerank_candidates);
    if count < 2 {
      return;
    }
    let mut top: Vec<SearchHit> = hits.drain(..count).collect();
    let documents: Vec<String> =
      top.iter().map(|hit| hit.memory.content.clone()).collect();

    match self.reranker.rerank(query, &documents).await {
      Ok(scores) if scores.len() == top.len() => {
        let mut scored: Vec<(f32, SearchHit)> =
          scores.into_iter().zip(top).collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        top = scored.into_iter().map(|(_, hit)| hit).collect();
      }
      Ok(scores) => tracing::warn!(
        got = scores.len(),
        wanted = top.len(),
        "the reranker returned the wrong number of scores, keeping the fused \
         order"
      ),
      Err(error) => tracing::warn!(
        %error,
        "the reranker failed, keeping the fused order"
      ),
    }

    top.append(hits);
    *hits = top;
  }

  async fn memory_text(
    &self,
    text: String,
    vector: Vector,
  ) -> Result<MemoryText, StoreError> {
    let interests = self.store.interests().await?;
    Ok(MemoryText {
      hash: format!("{:x}", Sha256::digest(text.as_bytes())),
      interest_score: ranking::interest_score(&vector, &interests),
      text,
      vector,
    })
  }

  async fn embed(
    &self,
    kind: EmbedKind,
    text: &str,
  ) -> Result<Vector, ServiceError> {
    let vectors = self.embedder.embed(kind, &[text.to_owned()]).await?;
    let [vector] = <[Vector; 1]>::try_from(vectors)
      .map_err(|vectors| ServiceError::VectorCount { got: vectors.len() })?;
    let expected = self.embedder.fingerprint().dimension;
    if vector.len() != expected {
      return Err(ServiceError::VectorLength {
        got: vector.len(),
        expected,
      });
    }
    Ok(vector)
  }

  fn check_text(
    &self,
    what: &'static str,
    text: &str,
  ) -> Result<(), ServiceError> {
    if text.trim().is_empty() {
      return Err(ServiceError::Empty { what });
    }
    let length = text.chars().count();
    let limit = self.write.max_text_chars;
    if length > limit {
      return Err(ServiceError::TooLong {
        what,
        length,
        limit,
      });
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use async_trait::async_trait;
  use serde_json::Value;

  use super::*;
  use crate::fakes::{FakeEmbedder, FakeReranker, FakeStore};
  use crate::reranker::{NoReranker, RerankError};
  use crate::types::Version;

  /// Gives the documents the scores from a fixed list.
  struct FixedScores(Vec<f32>);

  #[async_trait]
  impl Reranker for FixedScores {
    async fn rerank(
      &self,
      _query: &str,
      _documents: &[String],
    ) -> Result<Vec<f32>, RerankError> {
      Ok(self.0.clone())
    }
  }

  struct BrokenReranker;

  #[async_trait]
  impl Reranker for BrokenReranker {
    async fn rerank(
      &self,
      _query: &str,
      _documents: &[String],
    ) -> Result<Vec<f32>, RerankError> {
      Err(RerankError::BadResponse("the reranker is down".to_owned()))
    }
  }

  type TestService<R> = Service<FakeEmbedder, R, FakeStore>;

  fn service<R: Reranker>(reranker: R) -> TestService<R> {
    let embedder = FakeEmbedder::new(4)
      .with_word("cat", vec![1.0, 0.0, 0.0, 0.0])
      .with_word("dog", vec![0.0, 1.0, 0.0, 0.0])
      .with_word("tax", vec![0.0, 0.0, 1.0, 0.0]);
    let write = WriteConfig {
      max_text_chars: 60,
      ..WriteConfig::default()
    };
    Service::new(
      embedder,
      reranker,
      FakeStore::new(),
      write,
      SearchConfig::default(),
    )
  }

  fn event(content: &str) -> NewEvent {
    NewEvent {
      content: content.to_owned(),
      metadata: Value::Null,
    }
  }

  fn written(outcome: StoreOutcome) -> MemoryId {
    match outcome {
      StoreOutcome::Written { memory_id } => memory_id,
      other => panic!("expected a write, got {other:?}"),
    }
  }

  fn candidates(outcome: StoreOutcome) -> Vec<Candidate> {
    match outcome {
      StoreOutcome::NeedsDecision { candidates } => candidates,
      other => panic!("expected a question, got {other:?}"),
    }
  }

  /// Makes a memory without being asked about its neighbours.
  async fn add<R: Reranker>(
    service: &TestService<R>,
    content: &str,
  ) -> MemoryId {
    written(
      service
        .store(event(content), Some(Decision::New))
        .await
        .unwrap(),
    )
  }

  async fn recall<R: Reranker>(
    service: &TestService<R>,
    id: MemoryId,
  ) -> Recall {
    service.recall(id, true).await.unwrap().unwrap()
  }

  #[tokio::test]
  async fn an_event_with_no_neighbours_is_written_at_once() {
    let service = service(NoReranker);
    let id = written(service.store(event("cat sleeps"), None).await.unwrap());

    let recalled = recall(&service, id).await;
    assert_eq!(recalled.memory.content, "cat sleeps");
    assert_eq!(recalled.memory.event_count, 1);
    assert_eq!(recalled.memory.interest_score, None);
    let events = recalled.events.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].memory_text, "cat sleeps");
  }

  #[tokio::test]
  async fn an_unrelated_event_makes_its_own_memory() {
    let service = service(NoReranker);
    let cat = add(&service, "cat sleeps").await;
    let dog = written(service.store(event("dog barks"), None).await.unwrap());
    assert_ne!(cat, dog);
  }

  #[tokio::test]
  async fn a_near_event_gets_the_neighbours_back_and_writes_nothing() {
    let service = service(NoReranker);
    let id = add(&service, "cat sleeps").await;

    let found =
      candidates(service.store(event("cat purrs"), None).await.unwrap());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].memory.id, id);
    assert_eq!(found[0].memory.version, Version::new(1));

    let recalled = recall(&service, id).await;
    assert_eq!(recalled.memory.event_count, 1);
    assert_eq!(recalled.events.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn attach_rewrites_the_memory_and_keeps_both_texts_in_the_log() {
    let service = service(NoReranker);
    let id = add(&service, "cat sleeps").await;

    let outcome = service
      .store(
        event("cat purrs"),
        Some(Decision::Attach {
          memory_id: id,
          version: Version::new(1),
          memory_text: "cat sleeps and purrs".to_owned(),
        }),
      )
      .await
      .unwrap();
    assert_eq!(written(outcome), id);

    let recalled = recall(&service, id).await;
    assert_eq!(recalled.memory.content, "cat sleeps and purrs");
    assert_eq!(recalled.memory.event_count, 2);
    assert_eq!(recalled.memory.version, Version::new(2));
    let events = recalled.events.unwrap();
    assert_eq!(events[0].content, "cat purrs");
    assert_eq!(events[0].memory_text, "cat sleeps and purrs");
    assert_eq!(events[1].memory_text, "cat sleeps");
  }

  #[tokio::test]
  async fn repeat_counts_the_event_and_keeps_the_text() {
    let service = service(NoReranker);
    let id = add(&service, "cat sleeps").await;

    let outcome = service
      .store(
        event("the cat is sleeping again"),
        Some(Decision::Repeat {
          memory_id: id,
          version: Version::new(1),
        }),
      )
      .await
      .unwrap();
    assert_eq!(written(outcome), id);

    let recalled = recall(&service, id).await;
    assert_eq!(recalled.memory.content, "cat sleeps");
    assert_eq!(recalled.memory.event_count, 2);
    assert_eq!(recalled.events.unwrap()[0].memory_text, "cat sleeps");
  }

  #[tokio::test]
  async fn a_stale_version_asks_again_and_writes_nothing() {
    let service = service(NoReranker);
    let id = add(&service, "cat sleeps").await;

    let outcome = service
      .store(
        event("cat purrs"),
        Some(Decision::Repeat {
          memory_id: id,
          version: Version::new(99),
        }),
      )
      .await
      .unwrap();
    let found = candidates(outcome);
    assert_eq!(found[0].memory.id, id);
    assert_eq!(recall(&service, id).await.events.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn a_new_decision_makes_a_second_memory_beside_a_near_one() {
    let service = service(NoReranker);
    let first = add(&service, "cat sleeps").await;
    let second = written(
      service
        .store(event("cat purrs"), Some(Decision::New))
        .await
        .unwrap(),
    );
    assert_ne!(first, second);
  }

  #[tokio::test]
  async fn text_that_is_already_a_memory_asks_instead_of_duplicating() {
    let service = service(NoReranker);
    let id = add(&service, "cat sleeps").await;

    let outcome = service
      .store(event("cat sleeps"), Some(Decision::New))
      .await
      .unwrap();
    let found = candidates(outcome);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].memory.id, id);
    assert!(found[0].distance < 1e-4);
  }

  #[tokio::test]
  async fn a_forgotten_memory_is_offered_and_comes_back_when_attached() {
    let service = service(NoReranker);
    let id = add(&service, "cat sleeps").await;
    service.store.forget(id).await;

    assert!(service.recall(id, false).await.unwrap().is_none());
    assert!(service.search("cat", None).await.unwrap().is_empty());

    let found =
      candidates(service.store(event("cat purrs"), None).await.unwrap());
    assert_eq!(found[0].memory.id, id);
    assert!(found[0].memory.forgotten_at.is_some());

    let outcome = service
      .store(
        event("cat purrs"),
        Some(Decision::Attach {
          memory_id: id,
          version: found[0].memory.version,
          memory_text: "cat sleeps and purrs".to_owned(),
        }),
      )
      .await
      .unwrap();
    assert_eq!(written(outcome), id);
    assert!(service.recall(id, false).await.unwrap().is_some());
    assert_eq!(service.search("cat", None).await.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn long_and_empty_text_is_refused() {
    let service = service(NoReranker);
    let long = "cat ".repeat(20);

    let error = service.store(event(&long), None).await.unwrap_err();
    assert!(matches!(error, ServiceError::TooLong { .. }), "{error}");
    assert!(error.to_string().contains("condense"), "{error}");

    let error = service.store(event("  "), None).await.unwrap_err();
    assert!(matches!(error, ServiceError::Empty { .. }), "{error}");

    let id = add(&service, "cat sleeps").await;
    let error = service
      .store(
        event("cat purrs"),
        Some(Decision::Attach {
          memory_id: id,
          version: Version::new(1),
          memory_text: long,
        }),
      )
      .await
      .unwrap_err();
    assert!(error.to_string().contains("condense"), "{error}");
  }

  #[tokio::test]
  async fn a_new_memory_is_scored_against_the_interests() {
    let service = service(NoReranker);
    service
      .store
      .set_interests(vec![vec![1.0, 0.0, 0.0, 0.0]])
      .await;

    let cat = add(&service, "cat sleeps").await;
    let dog = add(&service, "dog barks").await;

    let cat = recall(&service, cat).await.memory.interest_score.unwrap();
    let dog = recall(&service, dog).await.memory.interest_score.unwrap();
    assert!(cat > 0.99, "{cat}");
    assert!(dog < 0.1, "{dog}");
  }

  #[tokio::test]
  async fn search_finds_by_meaning_and_by_word() {
    let service = service(NoReranker);
    add(&service, "cat sleeps").await;
    add(&service, "dog barks").await;
    add(&service, "tax is due").await;

    let by_meaning = service.search("cat", None).await.unwrap();
    assert_eq!(by_meaning[0].memory.content, "cat sleeps");

    let by_word = service.search("barks", None).await.unwrap();
    assert_eq!(by_word[0].memory.content, "dog barks");
  }

  #[tokio::test]
  async fn search_honours_the_limit() {
    let service = service(NoReranker);
    add(&service, "cat sleeps").await;
    add(&service, "dog barks").await;
    add(&service, "tax is due").await;

    assert_eq!(service.search("cat", Some(2)).await.unwrap().len(), 2);
    assert_eq!(service.search("cat", None).await.unwrap().len(), 3);
  }

  #[tokio::test]
  async fn an_empty_query_is_refused() {
    let service = service(NoReranker);
    let error = service.search(" ", None).await.unwrap_err();
    assert!(matches!(error, ServiceError::Empty { .. }), "{error}");
  }

  #[tokio::test]
  async fn search_changes_nothing() {
    let service = service(NoReranker);
    let id = add(&service, "cat sleeps").await;
    let before = recall(&service, id).await;
    service.search("cat", None).await.unwrap();
    assert_eq!(recall(&service, id).await, before);
  }

  async fn order<R: Reranker>(service: &TestService<R>) -> Vec<MemoryId> {
    add(service, "cat sleeps").await;
    add(service, "cat purrs").await;
    service
      .search("cat", None)
      .await
      .unwrap()
      .iter()
      .map(|hit| hit.memory.id)
      .collect()
  }

  #[tokio::test]
  async fn the_reranker_decides_the_order_of_the_top() {
    let kept = order(&service(NoReranker)).await;
    let flipped = order(&service(FixedScores(vec![0.0, 1.0]))).await;
    assert_eq!(kept.len(), 2);
    assert_eq!(flipped, [kept[1], kept[0]]);
  }

  #[tokio::test]
  async fn a_reranker_that_reads_the_words_can_pick_the_best_match() {
    let service = service(FakeReranker);
    add(&service, "cat sleeps").await;
    add(&service, "cat purrs").await;
    let hits = service.search("cat purrs", None).await.unwrap();
    assert_eq!(hits[0].memory.content, "cat purrs");
  }

  #[tokio::test]
  async fn a_broken_reranker_leaves_the_fused_order() {
    let kept = order(&service(NoReranker)).await;
    let broken = order(&service(BrokenReranker)).await;
    assert_eq!(broken, kept);
  }

  #[tokio::test]
  async fn the_wrong_number_of_scores_leaves_the_fused_order() {
    let kept = order(&service(NoReranker)).await;
    let short = order(&service(FixedScores(vec![1.0]))).await;
    assert_eq!(short, kept);
  }

  #[tokio::test]
  async fn recall_without_events_leaves_them_out_and_unknown_ids_are_none() {
    let service = service(NoReranker);
    let id = add(&service, "cat sleeps").await;
    assert!(
      service
        .recall(id, false)
        .await
        .unwrap()
        .unwrap()
        .events
        .is_none()
    );
    let unknown = MemoryId::new(id.get().saturating_add(1));
    assert!(service.recall(unknown, true).await.unwrap().is_none());
  }
}
