//! The checks every `Store` has to pass. Each backend runs them from its own
//! test file, on stores that are migrated and empty and whose vectors are
//! `DIMENSION` numbers long.

#![allow(
  clippy::unwrap_used,
  clippy::panic,
  clippy::indexing_slicing,
  reason = "the checks fail by panicking, like tests do, but this is a library \
            and clippy cannot tell"
)]

use futures_util::future::join;
use mcp_semantic_memory::store::{
  Consolidated, Consolidation, Forgotten, MemoryText, NewInterest, Store,
  Verdict, Write, WriteOutcome,
};
use mcp_semantic_memory::types::{
  Fingerprint, MemoryId, NewEvent, Vector, Version,
};
use serde_json::{Value, json};

/// How long the vectors of the stores under test are.
pub const DIMENSION: usize = 3;

/// The fingerprint every store under test is made with.
pub fn fingerprint() -> Fingerprint {
  Fingerprint {
    model: "test".to_owned(),
    dimension: DIMENSION,
  }
}

/// Runs every check. `fresh` makes a new empty store for each check.
pub async fn run(fresh: impl AsyncFn() -> Box<dyn Store>) {
  migrating_twice(&*fresh().await).await;
  a_new_memory_can_be_read_back(&*fresh().await).await;
  the_same_text_twice_is_a_duplicate(&*fresh().await).await;
  nearest_orders_by_distance(&*fresh().await).await;
  attach_replaces_text_and_vector(&*fresh().await).await;
  a_stale_version_is_a_conflict(&*fresh().await).await;
  attaching_a_taken_text_is_a_duplicate(&*fresh().await).await;
  attaching_to_a_missing_memory_is_an_error(&*fresh().await).await;
  repeat_keeps_the_text(&*fresh().await).await;
  events_are_newest_first_and_capped(&*fresh().await).await;
  keyword_finds_words(&*fresh().await).await;
  keyword_follows_attached_text(&*fresh().await).await;
  no_interests_at_first(&*fresh().await).await;
  a_new_store_is_healthy(&*fresh().await).await;
  the_sample_is_the_most_reinforced_first(&*fresh().await).await;
  the_active_memories_come_in_pages(&*fresh().await).await;
  consolidating_writes_interests_scores_and_forgetting(&*fresh().await).await;
  a_later_consolidation_replaces_the_interests(&*fresh().await).await;
  a_verdict_for_a_changed_memory_is_left_out(&*fresh().await).await;
  a_forgotten_memory_comes_back_when_an_event_attaches(&*fresh().await).await;
  racing_writers_of_one_text_make_one_memory(&*fresh().await).await;
  racing_attaches_on_one_version_have_one_winner(&*fresh().await).await;
}

fn event(content: &str) -> NewEvent {
  NewEvent {
    content: content.to_owned(),
    metadata: Value::Null,
  }
}

fn memory_text(text: &str, vector: Vector) -> MemoryText {
  MemoryText {
    text: text.to_owned(),
    hash: format!("hash of {text}"),
    vector,
    interest_score: None,
  }
}

fn written(outcome: WriteOutcome) -> MemoryId {
  match outcome {
    WriteOutcome::Written(id) => id,
    other => panic!("expected a write, got {other:?}"),
  }
}

pub(crate) async fn add(
  store: &dyn Store,
  text: &str,
  vector: Vector,
) -> MemoryId {
  let write = Write::New {
    event: event(text),
    memory: memory_text(text, vector),
  };
  written(store.write(write).await.unwrap())
}

async fn migrating_twice(store: &dyn Store) {
  store.migrate().await.unwrap();
  add(store, "still works", vec![1.0, 0.0, 0.0]).await;
}

async fn a_new_memory_can_be_read_back(store: &dyn Store) {
  let write = Write::New {
    event: NewEvent {
      content: "met Ada".to_owned(),
      metadata: json!({ "session": "one" }),
    },
    memory: MemoryText {
      text: "Ada is a friend".to_owned(),
      hash: "ada".to_owned(),
      vector: vec![1.0, 0.0, 0.0],
      interest_score: Some(0.5),
    },
  };
  let id = written(store.write(write).await.unwrap());

  let memory = store.memory(id).await.unwrap().unwrap();
  assert_eq!(memory.content, "Ada is a friend");
  assert_eq!(memory.content_hash, "ada");
  assert_eq!(memory.event_count, 1);
  assert_eq!(memory.version, Version::new(1));
  assert!(memory.forgotten_at.is_none());
  assert!((memory.interest_score.unwrap() - 0.5).abs() < 1e-6);

  let events = store.events(id, 10).await.unwrap();
  assert_eq!(events.len(), 1);
  assert_eq!(events[0].memory_id, id);
  assert_eq!(events[0].content, "met Ada");
  assert_eq!(events[0].metadata, json!({ "session": "one" }));
  assert_eq!(events[0].memory_text, "Ada is a friend");

  let unknown = MemoryId::new(id.get().saturating_add(1000));
  assert!(store.memory(unknown).await.unwrap().is_none());
}

async fn the_same_text_twice_is_a_duplicate(store: &dyn Store) {
  let id = add(store, "cat sleeps", vec![1.0, 0.0, 0.0]).await;
  let write = Write::New {
    event: event("cat sleeps"),
    memory: memory_text("cat sleeps", vec![1.0, 0.0, 0.0]),
  };
  assert_eq!(
    store.write(write).await.unwrap(),
    WriteOutcome::Duplicate(id)
  );
  assert_eq!(store.events(id, 10).await.unwrap().len(), 1);
}

async fn nearest_orders_by_distance(store: &dyn Store) {
  let a = add(store, "a", vec![1.0, 0.0, 0.0]).await;
  let b = add(store, "b", vec![0.8, 0.2, 0.0]).await;
  let c = add(store, "c", vec![0.0, 0.0, 1.0]).await;

  let found = store
    .nearest(&[1.0, 0.0, 0.0], 2, Forgotten::Skip)
    .await
    .unwrap();
  let ids: Vec<MemoryId> = found.iter().map(|hit| hit.memory.id).collect();
  assert_eq!(ids, [a, b]);
  assert!(found[0].distance < 1e-4);
  assert!(found[0].distance <= found[1].distance);

  let found = store
    .nearest(&[1.0, 0.0, 0.0], 10, Forgotten::Skip)
    .await
    .unwrap();
  let ids: Vec<MemoryId> = found.iter().map(|hit| hit.memory.id).collect();
  assert_eq!(ids, [a, b, c]);
  assert!((found[2].distance - 1.0).abs() < 1e-4);

  let found = store
    .nearest(&[1.0, 0.0, 0.0], 0, Forgotten::Skip)
    .await
    .unwrap();
  assert!(found.is_empty());
}

async fn attach_replaces_text_and_vector(store: &dyn Store) {
  let id = add(store, "cat sleeps", vec![1.0, 0.0, 0.0]).await;

  let write = Write::Attach {
    memory_id: id,
    version: Version::new(1),
    event: event("cat purrs"),
    memory: memory_text("cat sleeps and purrs", vec![0.0, 1.0, 0.0]),
  };
  assert_eq!(store.write(write).await.unwrap(), WriteOutcome::Written(id));

  let memory = store.memory(id).await.unwrap().unwrap();
  assert_eq!(memory.content, "cat sleeps and purrs");
  assert_eq!(memory.content_hash, "hash of cat sleeps and purrs");
  assert_eq!(memory.version, Version::new(2));
  assert_eq!(memory.event_count, 2);

  let found = store
    .nearest(&[0.0, 1.0, 0.0], 1, Forgotten::Skip)
    .await
    .unwrap();
  assert_eq!(found[0].memory.id, id);
  assert!(found[0].distance < 1e-4);
  let found = store
    .nearest(&[1.0, 0.0, 0.0], 1, Forgotten::Skip)
    .await
    .unwrap();
  assert!(found[0].distance > 0.9);

  let events = store.events(id, 10).await.unwrap();
  assert_eq!(events.len(), 2);
  assert_eq!(events[0].content, "cat purrs");
  assert_eq!(events[0].memory_text, "cat sleeps and purrs");
  assert_eq!(events[1].content, "cat sleeps");
  assert_eq!(events[1].memory_text, "cat sleeps");
}

async fn a_stale_version_is_a_conflict(store: &dyn Store) {
  let id = add(store, "cat sleeps", vec![1.0, 0.0, 0.0]).await;

  let attach = Write::Attach {
    memory_id: id,
    version: Version::new(7),
    event: event("cat purrs"),
    memory: memory_text("cat purrs", vec![0.0, 1.0, 0.0]),
  };
  assert_eq!(store.write(attach).await.unwrap(), WriteOutcome::Conflict);
  let repeat = Write::Repeat {
    memory_id: id,
    version: Version::new(7),
    event: event("cat sleeps again"),
  };
  assert_eq!(store.write(repeat).await.unwrap(), WriteOutcome::Conflict);

  let memory = store.memory(id).await.unwrap().unwrap();
  assert_eq!(memory.content, "cat sleeps");
  assert_eq!(memory.version, Version::new(1));
  assert_eq!(memory.event_count, 1);
  assert_eq!(store.events(id, 10).await.unwrap().len(), 1);
}

async fn attaching_a_taken_text_is_a_duplicate(store: &dyn Store) {
  let a = add(store, "a", vec![1.0, 0.0, 0.0]).await;
  let b = add(store, "b", vec![0.0, 1.0, 0.0]).await;

  let write = Write::Attach {
    memory_id: a,
    version: Version::new(1),
    event: event("a is b"),
    memory: memory_text("b", vec![0.0, 1.0, 0.0]),
  };
  assert_eq!(
    store.write(write).await.unwrap(),
    WriteOutcome::Duplicate(b)
  );

  let memory = store.memory(a).await.unwrap().unwrap();
  assert_eq!(memory.content, "a");
  assert_eq!(memory.version, Version::new(1));
  assert_eq!(store.events(a, 10).await.unwrap().len(), 1);
}

async fn attaching_to_a_missing_memory_is_an_error(store: &dyn Store) {
  let write = Write::Attach {
    memory_id: MemoryId::new(424_242),
    version: Version::new(1),
    event: event("lost"),
    memory: memory_text("lost", vec![1.0, 0.0, 0.0]),
  };
  assert!(store.write(write).await.is_err());
}

async fn repeat_keeps_the_text(store: &dyn Store) {
  let id = add(store, "cat sleeps", vec![1.0, 0.0, 0.0]).await;

  let write = Write::Repeat {
    memory_id: id,
    version: Version::new(1),
    event: event("the cat sleeps again"),
  };
  assert_eq!(store.write(write).await.unwrap(), WriteOutcome::Written(id));

  let memory = store.memory(id).await.unwrap().unwrap();
  assert_eq!(memory.content, "cat sleeps");
  assert_eq!(memory.version, Version::new(2));
  assert_eq!(memory.event_count, 2);

  let events = store.events(id, 10).await.unwrap();
  assert_eq!(events[0].content, "the cat sleeps again");
  assert_eq!(events[0].memory_text, "cat sleeps");
}

async fn events_are_newest_first_and_capped(store: &dyn Store) {
  let id = add(store, "cat sleeps", vec![1.0, 0.0, 0.0]).await;
  for n in 1..=3 {
    let write = Write::Repeat {
      memory_id: id,
      version: Version::new(n),
      event: event(&format!("repeat {n}")),
    };
    assert_eq!(store.write(write).await.unwrap(), WriteOutcome::Written(id));
  }

  let events = store.events(id, 2).await.unwrap();
  let contents: Vec<&str> =
    events.iter().map(|event| event.content.as_str()).collect();
  assert_eq!(contents, ["repeat 3", "repeat 2"]);
}

async fn keyword_finds_words(store: &dyn Store) {
  let cat = add(store, "the cat sat on the mat", vec![1.0, 0.0, 0.0]).await;
  add(store, "the dog sat", vec![0.0, 1.0, 0.0]).await;
  add(store, "tax return due", vec![0.0, 0.0, 1.0]).await;

  let found = store.keyword("cat mat", 10).await.unwrap();
  assert_eq!(found[0].id, cat);
  assert_eq!(store.keyword("sat", 10).await.unwrap().len(), 2);
  assert_eq!(store.keyword("sat", 1).await.unwrap().len(), 1);
  assert_eq!(store.keyword("CAT", 10).await.unwrap()[0].id, cat);
  assert!(store.keyword("zebra", 10).await.unwrap().is_empty());
  assert!(store.keyword("", 10).await.unwrap().is_empty());

  let found = store.keyword("\"cat\" OR (NEAR* ' -", 10).await.unwrap();
  assert_eq!(found[0].id, cat);
}

async fn keyword_follows_attached_text(store: &dyn Store) {
  let id = add(store, "cat sleeps", vec![1.0, 0.0, 0.0]).await;
  let write = Write::Attach {
    memory_id: id,
    version: Version::new(1),
    event: event("it was a dog"),
    memory: memory_text("dog barks", vec![0.0, 1.0, 0.0]),
  };
  assert_eq!(store.write(write).await.unwrap(), WriteOutcome::Written(id));

  assert!(store.keyword("cat", 10).await.unwrap().is_empty());
  assert_eq!(store.keyword("dog", 10).await.unwrap()[0].id, id);
}

async fn no_interests_at_first(store: &dyn Store) {
  assert!(store.interests().await.unwrap().is_empty());
}

async fn repeat(store: &dyn Store, id: MemoryId, version: i64) {
  let write = Write::Repeat {
    memory_id: id,
    version: Version::new(version),
    event: event("again"),
  };
  assert_eq!(store.write(write).await.unwrap(), WriteOutcome::Written(id));
}

fn near(vector: &[f32], expected: &[f32]) -> bool {
  vector.len() == expected.len()
    && vector
      .iter()
      .zip(expected)
      .all(|(a, b)| (a - b).abs() < 1e-4)
}

async fn the_sample_is_the_most_reinforced_first(store: &dyn Store) {
  let a = add(store, "a", vec![1.0, 0.0, 0.0]).await;
  let b = add(store, "b", vec![0.0, 1.0, 0.0]).await;
  let c = add(store, "c", vec![0.0, 0.0, 1.0]).await;
  repeat(store, b, 1).await;
  repeat(store, b, 2).await;
  repeat(store, a, 1).await;

  let sample = store.sample(2).await.unwrap();
  let ids: Vec<MemoryId> = sample.iter().map(|item| item.memory.id).collect();
  assert_eq!(ids, [b, a]);
  assert!(near(&sample[0].vector, &[0.0, 1.0, 0.0]), "{:?}", sample[0]);

  let all = store.sample(10).await.unwrap();
  assert_eq!(all.len(), 3);
  assert_eq!(all[2].memory.id, c);
  assert!(store.sample(0).await.unwrap().is_empty());
}

async fn the_active_memories_come_in_pages(store: &dyn Store) {
  let mut ids = Vec::new();
  for n in 0..5_u8 {
    let vector = vec![1.0, f32::from(n), 0.0];
    ids.push(add(store, &format!("memory {n}"), vector).await);
  }

  let first = store.active(None, 2).await.unwrap();
  assert_eq!(first.len(), 2);
  assert!(near(&first[1].vector, &[1.0, 1.0, 0.0]), "{:?}", first[1]);

  let mut seen = Vec::new();
  let mut after = None;
  loop {
    let page = store.active(after, 2).await.unwrap();
    let Some(last) = page.last() else {
      break;
    };
    after = Some(last.memory.id);
    seen.extend(page.iter().map(|item| item.memory.id));
  }
  assert_eq!(seen, ids);
}

fn verdict(id: MemoryId, version: i64, score: f32, forget: bool) -> Verdict {
  Verdict {
    memory_id: id,
    version: Version::new(version),
    interest_score: score,
    forget,
  }
}

fn interests() -> Vec<NewInterest> {
  vec![
    NewInterest {
      top_terms: "cat fur".to_owned(),
      centre: vec![1.0, 0.0, 0.0],
    },
    NewInterest {
      top_terms: "dog".to_owned(),
      centre: vec![0.0, 1.0, 0.0],
    },
  ]
}

async fn consolidating_writes_interests_scores_and_forgetting(
  store: &dyn Store,
) {
  let a = add(store, "a", vec![1.0, 0.0, 0.0]).await;
  let b = add(store, "b", vec![0.0, 1.0, 0.0]).await;
  let run = Consolidation {
    interests: interests(),
    verdicts: vec![verdict(a, 1, 0.9, false), verdict(b, 1, 0.1, true)],
  };
  assert_eq!(
    store.consolidate(run, None).await.unwrap(),
    Consolidated::Written
  );

  let centres = store.interests().await.unwrap();
  assert_eq!(centres.len(), 2);
  assert!(near(&centres[0], &[1.0, 0.0, 0.0]), "{centres:?}");
  assert!(near(&centres[1], &[0.0, 1.0, 0.0]), "{centres:?}");

  let kept = store.memory(a).await.unwrap().unwrap();
  assert!((kept.interest_score.unwrap() - 0.9).abs() < 1e-5);
  assert!(kept.forgotten_at.is_none());
  let forgotten = store.memory(b).await.unwrap().unwrap();
  assert!((forgotten.interest_score.unwrap() - 0.1).abs() < 1e-5);
  assert!(forgotten.forgotten_at.is_some());
  assert_eq!(forgotten.version, Version::new(1));

  let found = store
    .nearest(&[0.0, 1.0, 0.0], 10, Forgotten::Skip)
    .await
    .unwrap();
  assert_eq!(found.len(), 1);
  assert_eq!(found[0].memory.id, a);
  assert!(store.keyword("b", 10).await.unwrap().is_empty());
  assert_eq!(store.active(None, 10).await.unwrap().len(), 1);

  let health = store.health().await.unwrap();
  assert_eq!(health.interests_to_rebuild, 0);
  assert_eq!(health.memories_to_reembed, 0);
  let since = health.since_consolidation.unwrap();
  assert!(since < std::time::Duration::from_secs(60), "{since:?}");
}

async fn a_later_consolidation_replaces_the_interests(store: &dyn Store) {
  let a = add(store, "a", vec![1.0, 0.0, 0.0]).await;
  let first = Consolidation {
    interests: interests(),
    verdicts: vec![verdict(a, 1, 0.5, false)],
  };
  store.consolidate(first, None).await.unwrap();

  let second = Consolidation {
    interests: interests().into_iter().take(1).collect(),
    verdicts: vec![verdict(a, 1, 0.8, false)],
  };
  store.consolidate(second, None).await.unwrap();

  assert_eq!(store.interests().await.unwrap().len(), 1);
  let memory = store.memory(a).await.unwrap().unwrap();
  assert!((memory.interest_score.unwrap() - 0.8).abs() < 1e-5);
}

async fn a_verdict_for_a_changed_memory_is_left_out(store: &dyn Store) {
  let id = add(store, "a", vec![1.0, 0.0, 0.0]).await;
  repeat(store, id, 1).await;

  let run = Consolidation {
    interests: interests(),
    verdicts: vec![verdict(id, 1, 0.5, true)],
  };
  store.consolidate(run, None).await.unwrap();

  let memory = store.memory(id).await.unwrap().unwrap();
  assert!(memory.forgotten_at.is_none());
  assert_eq!(memory.interest_score, None);
}

async fn a_forgotten_memory_comes_back_when_an_event_attaches(
  store: &dyn Store,
) {
  let id = add(store, "a", vec![1.0, 0.0, 0.0]).await;
  let run = Consolidation {
    interests: interests(),
    verdicts: vec![verdict(id, 1, 0.0, true)],
  };
  store.consolidate(run, None).await.unwrap();
  assert!(
    store
      .memory(id)
      .await
      .unwrap()
      .unwrap()
      .forgotten_at
      .is_some()
  );

  repeat(store, id, 1).await;
  let memory = store.memory(id).await.unwrap().unwrap();
  assert!(memory.forgotten_at.is_none());
  assert_eq!(store.active(None, 10).await.unwrap().len(), 1);
}

async fn a_new_store_is_healthy(store: &dyn Store) {
  let empty = store.health().await.unwrap();
  assert_eq!(empty.memories_to_reembed, 0);
  assert_eq!(empty.interests_to_rebuild, 0);
  assert_eq!(empty.since_consolidation, None);

  add(store, "cat", vec![1.0, 0.0, 0.0]).await;
  let written = store.health().await.unwrap();
  assert_eq!(written.memories_to_reembed, 0);
  assert_eq!(written.interests_to_rebuild, 0);
  let since = written.since_consolidation.unwrap();
  assert!(since < std::time::Duration::from_secs(60), "{since:?}");
}

async fn racing_writers_of_one_text_make_one_memory(store: &dyn Store) {
  let write = || Write::New {
    event: event("cat sleeps"),
    memory: memory_text("cat sleeps", vec![1.0, 0.0, 0.0]),
  };
  let (first, second) = join(store.write(write()), store.write(write())).await;

  let mut outcomes = [first.unwrap(), second.unwrap()];
  outcomes.sort_by_key(|outcome| matches!(outcome, WriteOutcome::Duplicate(_)));
  let WriteOutcome::Written(id) = outcomes[0] else {
    panic!("expected one write, got {outcomes:?}");
  };
  assert_eq!(outcomes[1], WriteOutcome::Duplicate(id));
  assert_eq!(store.events(id, 10).await.unwrap().len(), 1);
}

async fn racing_attaches_on_one_version_have_one_winner(store: &dyn Store) {
  let id = add(store, "cat sleeps", vec![1.0, 0.0, 0.0]).await;
  let attach = |text: &str| Write::Attach {
    memory_id: id,
    version: Version::new(1),
    event: event(text),
    memory: memory_text(text, vec![0.0, 1.0, 0.0]),
  };
  let (first, second) = join(
    store.write(attach("cat purrs")),
    store.write(attach("cat hunts")),
  )
  .await;

  let outcomes = [first.unwrap(), second.unwrap()];
  let written = outcomes
    .iter()
    .filter(|outcome| **outcome == WriteOutcome::Written(id))
    .count();
  let conflicts = outcomes
    .iter()
    .filter(|outcome| **outcome == WriteOutcome::Conflict)
    .count();
  assert_eq!((written, conflicts), (1, 1), "{outcomes:?}");

  let memory = store.memory(id).await.unwrap().unwrap();
  assert_eq!(memory.version, Version::new(2));
  assert_eq!(memory.event_count, 2);
  assert_eq!(store.events(id, 10).await.unwrap().len(), 2);
}
