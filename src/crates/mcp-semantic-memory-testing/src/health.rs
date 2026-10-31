//! The checks of what `Store::health` reports, and of the consolidation that
//! depends on it. They need to change what a store holds behind its back,
//! which only the database can do, so each backend gives a `Tamper` for its
//! own database.

#![allow(
  clippy::unwrap_used,
  clippy::indexing_slicing,
  reason = "the checks fail by panicking, like tests do, but this is a library \
            and clippy cannot tell"
)]

use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::join;
use mcp_semantic_memory::store::{
  Consolidated, Consolidation, Forgotten, NewInterest, Store, Swapped, Verdict,
};
use mcp_semantic_memory::types::{Fingerprint, MemoryId, Version};

use crate::suite::{add, fingerprint};

const MINUTE: Duration = Duration::from_secs(60);
const DAY: Duration = Duration::from_secs(60 * 60 * 24);
const TWO_DAYS: Duration = DAY.saturating_mul(2);
const THREE_DAYS: Duration = DAY.saturating_mul(3);

/// The fingerprint of a model that is not the one under test.
const OTHER: &str = "other/3/cosine";

/// Changes what a store holds, the way only its database can.
#[async_trait]
pub trait Tamper: Send + Sync {
  /// Puts the vector of a memory back as another embedding model made it.
  async fn remake_vector(&self, id: MemoryId, fingerprint: &str);

  /// Takes the vector of a memory away.
  async fn drop_vector(&self, id: MemoryId);

  /// Forgets a memory the way consolidation would.
  async fn forget(&self, id: MemoryId);

  /// Adds the interests of a consolidation that ran `ago` before now, one for
  /// each fingerprint. An interest without a fingerprint has no vector.
  async fn add_interests(&self, ago: Duration, fingerprints: &[Option<&str>]);

  /// Makes every memory as old as `ago`, with its events left as they are.
  async fn age_memories(&self, ago: Duration);

  /// Moves everything that happened so far, the memories and their events,
  /// back in time by `ago`.
  async fn age_activity(&self, ago: Duration);

  /// A store on the same database, made with another embedding model.
  async fn reopen(&self, fingerprint: Fingerprint) -> Box<dyn Store>;
}

/// A store and the means to change what it holds.
pub type Subject = (Box<dyn Store>, Box<dyn Tamper>);

/// Runs every check. `fresh` makes a new empty store for each check.
pub async fn run(fresh: impl AsyncFn() -> Subject) {
  memories_made_by_another_model_need_a_reembed(fresh().await).await;
  a_memory_without_a_vector_needs_a_reembed(fresh().await).await;
  only_the_latest_interests_need_vectors(fresh().await).await;
  the_age_counts_from_the_oldest_memory_until_a_consolidation(fresh().await)
    .await;
  forgotten_memories_have_nothing_to_consolidate(fresh().await).await;
  a_quiet_spell_is_not_a_reason_to_consolidate(fresh().await).await;
  a_run_after_the_activity_leaves_nothing_to_do(fresh().await).await;
  activity_after_the_latest_run_is_counted(fresh().await).await;
  a_needed_consolidation_is_written_and_then_not_needed(fresh().await).await;
  two_consolidations_at_once_write_once(fresh().await).await;
  a_changed_model_is_fixed_by_a_reembed(fresh().await).await;
  a_reembed_of_another_model_starts_over(fresh().await).await;
}

fn model(name: &str, dimension: usize) -> Fingerprint {
  Fingerprint {
    model: name.to_owned(),
    dimension,
  }
}

async fn unembedded_ids(store: &dyn Store, limit: usize) -> Vec<MemoryId> {
  let memories = store.unembedded(limit).await.unwrap();
  memories.iter().map(|memory| memory.id).collect()
}

async fn a_changed_model_is_fixed_by_a_reembed((store, tamper): Subject) {
  let cat = add(&*store, "cat", vec![1.0, 0.0, 0.0]).await;
  let dog = add(&*store, "dog", vec![0.0, 1.0, 0.0]).await;
  let gone = add(&*store, "gone", vec![0.0, 0.0, 1.0]).await;
  tamper.forget(gone).await;
  let current = fingerprint().to_string();
  tamper.add_interests(MINUTE, &[Some(&current)]).await;

  let bigger = tamper.reopen(model("bigger", 5)).await;
  assert_eq!(bigger.health().await.unwrap().memories_to_reembed, 2);

  bigger.begin_reembed().await.unwrap();
  assert_eq!(unembedded_ids(&*bigger, 1).await, [cat]);
  let vector = vec![1.0, 0.0, 0.0, 0.0, 0.0];
  bigger.put_reembedded(vec![(cat, vector)]).await.unwrap();

  bigger.begin_reembed().await.unwrap();
  assert_eq!(unembedded_ids(&*bigger, 10).await, [dog, gone]);
  assert_eq!(bigger.swap_vectors().await.unwrap(), Swapped::Missing);
  assert_eq!(store.health().await.unwrap().memories_to_reembed, 0);

  let dog_vector = vec![0.0, 1.0, 0.0, 0.0, 0.0];
  let gone_vector = vec![0.0, 0.0, 1.0, 0.0, 0.0];
  bigger
    .put_reembedded(vec![(dog, dog_vector.clone()), (gone, gone_vector)])
    .await
    .unwrap();
  assert!(unembedded_ids(&*bigger, 10).await.is_empty());
  assert_eq!(bigger.swap_vectors().await.unwrap(), Swapped::Done);

  let health = bigger.health().await.unwrap();
  assert_eq!(health.memories_to_reembed, 0);
  assert_eq!(health.interests_to_rebuild, 1);
  assert!(bigger.interests().await.unwrap().is_empty());
  let found = bigger
    .nearest(&dog_vector, 3, Forgotten::Include)
    .await
    .unwrap();
  let ids: Vec<MemoryId> = found.iter().map(|hit| hit.memory.id).collect();
  assert_eq!(ids.len(), 3);
  assert_eq!(ids[0], dog);
  assert!(ids.contains(&gone));
}

async fn a_reembed_of_another_model_starts_over((store, tamper): Subject) {
  let cat = add(&*store, "cat", vec![1.0, 0.0, 0.0]).await;

  let abandoned = tamper.reopen(model("abandoned", 4)).await;
  abandoned.begin_reembed().await.unwrap();
  let vector = vec![1.0, 0.0, 0.0, 0.0];
  abandoned.put_reembedded(vec![(cat, vector)]).await.unwrap();

  let bigger = tamper.reopen(model("bigger", 5)).await;
  bigger.begin_reembed().await.unwrap();
  assert_eq!(unembedded_ids(&*bigger, 10).await, [cat]);
  let vector = vec![1.0, 0.0, 0.0, 0.0, 0.0];
  bigger.put_reembedded(vec![(cat, vector)]).await.unwrap();
  assert_eq!(bigger.swap_vectors().await.unwrap(), Swapped::Done);
  assert_eq!(bigger.health().await.unwrap().memories_to_reembed, 0);
}

async fn memories_made_by_another_model_need_a_reembed(
  (store, tamper): Subject,
) {
  add(&*store, "first", vec![1.0, 0.0, 0.0]).await;
  let second = add(&*store, "second", vec![0.0, 1.0, 0.0]).await;
  assert_eq!(store.health().await.unwrap().memories_to_reembed, 0);

  tamper.remake_vector(second, OTHER).await;
  assert_eq!(store.health().await.unwrap().memories_to_reembed, 1);

  tamper.forget(second).await;
  assert_eq!(store.health().await.unwrap().memories_to_reembed, 0);
}

async fn a_memory_without_a_vector_needs_a_reembed((store, tamper): Subject) {
  let id = add(&*store, "first", vec![1.0, 0.0, 0.0]).await;
  tamper.drop_vector(id).await;
  assert_eq!(store.health().await.unwrap().memories_to_reembed, 1);
}

async fn only_the_latest_interests_need_vectors((store, tamper): Subject) {
  let current = fingerprint().to_string();
  tamper.add_interests(TWO_DAYS, &[Some(OTHER)]).await;
  tamper.add_interests(DAY, &[Some(&current)]).await;
  assert_eq!(store.health().await.unwrap().interests_to_rebuild, 0);

  tamper
    .add_interests(MINUTE, &[Some(OTHER), None, Some(&current)])
    .await;
  assert_eq!(store.health().await.unwrap().interests_to_rebuild, 2);
}

async fn the_age_counts_from_the_oldest_memory_until_a_consolidation(
  (store, tamper): Subject,
) {
  add(&*store, "first", vec![1.0, 0.0, 0.0]).await;
  let fresh = store.health().await.unwrap().since_consolidation.unwrap();
  assert!(fresh < MINUTE, "{fresh:?}");

  tamper.age_memories(THREE_DAYS).await;
  let old = store.health().await.unwrap().since_consolidation.unwrap();
  assert!(about(old, THREE_DAYS), "{old:?}");

  let current = fingerprint().to_string();
  tamper.add_interests(DAY, &[Some(&current)]).await;
  let since = store.health().await.unwrap().since_consolidation.unwrap();
  assert!(about(since, DAY), "{since:?}");
}

/// Whether `age` is `expected`, give or take the little time the check itself
/// took.
fn about(age: Duration, expected: Duration) -> bool {
  age.abs_diff(expected) < MINUTE
}

async fn forgotten_memories_have_nothing_to_consolidate(
  (store, tamper): Subject,
) {
  let id = add(&*store, "first", vec![1.0, 0.0, 0.0]).await;
  tamper.forget(id).await;
  assert_eq!(store.health().await.unwrap().since_consolidation, None);
}

async fn a_quiet_spell_is_not_a_reason_to_consolidate(
  (store, tamper): Subject,
) {
  add(&*store, "first", vec![1.0, 0.0, 0.0]).await;
  tamper.age_activity(THREE_DAYS).await;
  let since = store.health().await.unwrap().since_consolidation.unwrap();
  assert!(since < MINUTE, "{since:?}");
}

async fn a_run_after_the_activity_leaves_nothing_to_do(
  (store, tamper): Subject,
) {
  add(&*store, "first", vec![1.0, 0.0, 0.0]).await;
  tamper.age_activity(THREE_DAYS).await;
  let current = fingerprint().to_string();
  tamper.add_interests(DAY, &[Some(&current)]).await;
  let since = store.health().await.unwrap().since_consolidation.unwrap();
  assert_eq!(since, Duration::ZERO);
}

async fn activity_after_the_latest_run_is_counted((store, tamper): Subject) {
  add(&*store, "first", vec![1.0, 0.0, 0.0]).await;
  tamper.age_activity(THREE_DAYS).await;
  let current = fingerprint().to_string();
  tamper
    .add_interests(DAY.saturating_mul(5), &[Some(&current)])
    .await;
  let since = store.health().await.unwrap().since_consolidation.unwrap();
  assert!(about(since, TWO_DAYS), "{since:?}");
}

fn consolidation(id: MemoryId) -> Consolidation {
  Consolidation {
    interests: vec![NewInterest {
      top_terms: "first".to_owned(),
      centre: vec![1.0, 0.0, 0.0],
    }],
    verdicts: vec![Verdict {
      memory_id: id,
      version: Version::new(1),
      interest_score: 1.0,
      forget: false,
    }],
  }
}

async fn a_needed_consolidation_is_written_and_then_not_needed(
  (store, tamper): Subject,
) {
  let id = add(&*store, "first", vec![1.0, 0.0, 0.0]).await;
  add(&*store, "second", vec![0.0, 1.0, 0.0]).await;

  let result = store.consolidate(consolidation(id), Some(DAY)).await;
  assert_eq!(result.unwrap(), Consolidated::NotStale);
  assert!(store.interests().await.unwrap().is_empty());

  tamper.age_memories(THREE_DAYS).await;
  let result = store.consolidate(consolidation(id), Some(DAY)).await;
  assert_eq!(result.unwrap(), Consolidated::Written);
  assert_eq!(store.interests().await.unwrap().len(), 1);

  let result = store.consolidate(consolidation(id), Some(DAY)).await;
  assert_eq!(result.unwrap(), Consolidated::NotStale);
  assert_eq!(store.interests().await.unwrap().len(), 1);
}

async fn two_consolidations_at_once_write_once((store, tamper): Subject) {
  let id = add(&*store, "first", vec![1.0, 0.0, 0.0]).await;
  add(&*store, "second", vec![0.0, 1.0, 0.0]).await;
  tamper.age_memories(THREE_DAYS).await;

  let (first, second) = join(
    store.consolidate(consolidation(id), Some(DAY)),
    store.consolidate(consolidation(id), Some(DAY)),
  )
  .await;

  let mut results = [first.unwrap(), second.unwrap()];
  results.sort_by_key(|result| *result == Consolidated::NotStale);
  assert_eq!(results, [Consolidated::Written, Consolidated::NotStale]);
  assert_eq!(store.interests().await.unwrap().len(), 1);
}
