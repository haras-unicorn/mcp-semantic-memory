//! The consolidation run: find the interests, score every memory against them
//! and forget what faded. It talks only to the `Store` facade, and it has no
//! model to talk to.
//!
//! Time here is relative, as in search. Recency is measured from the median
//! last event of the memories that were clustered, and the grace period from
//! the newest event, so a quiet spell ages nothing.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;

use crate::cluster::{Point, cluster_count, kmeans};
use crate::config::ConsolidationConfig;
use crate::ranking::{
  Rarity, frecency, interest_score, median_time, top_terms,
};
use crate::store::{
  Consolidated, Consolidation, Embedded, NewInterest, Store, StoreError,
  Verdict,
};
use crate::types::MemoryId;
use crate::vector::cosine_distance;

/// What to do with the result of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  /// Only report what would be written.
  DryRun,
  /// Write it. With a maximum age, the store checks under its lock that a
  /// consolidation is still needed for it, and writes nothing if another
  /// instance was first.
  Apply {
    /// The maximum age to check against, if the write is conditional.
    only_if_needed: Option<Duration>,
  },
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
  /// Nothing was written, as asked.
  DryRun,
  /// The interests, scores and forgetting are written.
  Written,
  /// Nothing was written, because another run came first and a consolidation
  /// is no longer needed.
  NotNeeded,
  /// There are no active memories.
  NothingToConsolidate,
}

/// One interest of a run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InterestReport {
  /// The words that name it, best first.
  pub top_terms: Vec<String>,
  /// How many of the clustered memories belong to it.
  pub memories: usize,
  /// The text of the memory closest to its centre.
  pub nearest: Option<String>,
}

/// A memory that a run forgets.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ForgottenMemory {
  /// Its id.
  pub memory_id: MemoryId,
  /// What it said.
  pub text: String,
}

/// What a run found and did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
  /// How the run ended.
  pub status: Status,
  /// How many active memories there were.
  pub memories: usize,
  /// The interests, whether written or not.
  pub interests: Vec<InterestReport>,
  /// The memories forgotten, or that would be.
  pub forgotten: Vec<ForgottenMemory>,
}

impl fmt::Display for Report {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let (verb, status) = match self.status {
      Status::DryRun => ("Would forget", "Dry run: nothing was written."),
      Status::Written => ("Forgot", "Written."),
      Status::NotNeeded => (
        "Would have forgotten",
        "Nothing was written: another run came first.",
      ),
      Status::NothingToConsolidate => {
        return writeln!(f, "There are no active memories to consolidate.");
      }
    };
    writeln!(f, "{status}")?;
    writeln!(
      f,
      "{} active memories, {} interests.",
      self.memories,
      self.interests.len()
    )?;
    for (number, interest) in self.interests.iter().enumerate() {
      writeln!(
        f,
        "\nInterest {}: {} ({} memories)",
        number.saturating_add(1),
        interest.top_terms.join(" "),
        interest.memories
      )?;
      if let Some(nearest) = &interest.nearest {
        writeln!(f, "  closest memory: {nearest}")?;
      }
    }
    writeln!(f, "\n{verb} {} memories.", self.forgotten.len())?;
    for memory in &self.forgotten {
      writeln!(f, "  {}: {}", memory.memory_id, memory.text)?;
    }
    Ok(())
  }
}

/// Runs a consolidation. The recency half-life is the one of search.
///
/// The clusters are found with nothing random, so with the same memories a
/// second run finds the same interests.
pub async fn consolidate<S: Store + ?Sized>(
  store: &S,
  config: &ConsolidationConfig,
  half_life: Duration,
  mode: Mode,
) -> Result<Report, StoreError> {
  let Some(Plan {
    consolidation,
    mut report,
  }) = plan(store, config, half_life).await?
  else {
    return Ok(Report {
      status: Status::NothingToConsolidate,
      memories: 0,
      interests: Vec::new(),
      forgotten: Vec::new(),
    });
  };

  report.status = match mode {
    Mode::DryRun => Status::DryRun,
    Mode::Apply { only_if_needed } => {
      match store.consolidate(consolidation, only_if_needed).await? {
        Consolidated::Written => Status::Written,
        Consolidated::NotStale => Status::NotNeeded,
      }
    }
  };
  Ok(report)
}

struct Plan {
  consolidation: Consolidation,
  report: Report,
}

/// Works out what a run would write, and writes nothing. The memories are
/// read in two passes of pages and never all at once: the first counts the
/// words and finds the newest event, the second scores each memory and decides
/// if it is forgotten. Between them the clusters are found from a sample.
async fn plan<S: Store + ?Sized>(
  store: &S,
  config: &ConsolidationConfig,
  half_life: Duration,
) -> Result<Option<Plan>, StoreError> {
  let mut rarity = Rarity::default();
  let mut newest: Option<DateTime<Utc>> = None;
  each_page(store, config.batch_size, |page| {
    for item in page {
      rarity.add(&item.memory.content);
      newest = newest.max(Some(item.memory.last_event_at));
    }
  })
  .await?;

  let sample = store.sample(config.sample_size).await?;
  let times: Vec<DateTime<Utc>> = sample
    .iter()
    .map(|item| item.memory.last_event_at)
    .collect();
  let (Some(newest), Some(reference)) = (newest, median_time(&times)) else {
    return Ok(None);
  };

  let weights: Vec<f32> = sample
    .iter()
    .map(|item| {
      frecency(
        item.memory.event_count,
        item.memory.last_event_at,
        reference,
        half_life,
      )
    })
    .collect();
  let points: Vec<Point<'_>> = sample
    .iter()
    .zip(&weights)
    .map(|(item, weight)| Point {
      vector: &item.vector,
      weight: *weight,
    })
    .collect();
  let k =
    cluster_count(rarity.memories(), config.k_base, config.k_min, config.k_max);
  let clusters = kmeans(&points, k, config.iterations);

  let mut interests = Vec::new();
  let mut interest_reports = Vec::new();
  for (index, centre) in clusters.centres.iter().enumerate() {
    let members: Vec<usize> = clusters
      .assignments
      .iter()
      .enumerate()
      .filter(|(_, assigned)| **assigned == index)
      .map(|(member, _)| member)
      .collect();
    let texts: Vec<(&str, f32)> = members
      .iter()
      .filter_map(|member| {
        Some((
          sample.get(*member)?.memory.content.as_str(),
          *weights.get(*member)?,
        ))
      })
      .collect();
    let terms = top_terms(&texts, &rarity, config.top_terms);
    let nearest = members
      .iter()
      .filter_map(|member| sample.get(*member))
      .min_by(|a, b| {
        cosine_distance(centre, &a.vector)
          .total_cmp(&cosine_distance(centre, &b.vector))
      });
    interests.push(NewInterest {
      top_terms: terms.join(" "),
      centre: centre.clone(),
    });
    interest_reports.push(InterestReport {
      top_terms: terms,
      memories: members.len(),
      nearest: nearest.map(|item| item.memory.content.clone()),
    });
  }

  let grace = TimeDelta::from_std(config.grace).unwrap_or(TimeDelta::MAX);
  let mut verdicts = Vec::new();
  let mut forgotten = Vec::new();
  each_page(store, config.batch_size, |page| {
    for item in page {
      let memory = &item.memory;
      let score =
        interest_score(&item.vector, &clusters.centres).unwrap_or(0.0);
      let faded = score < config.forget_below_interest
        && frecency(
          memory.event_count,
          memory.last_event_at,
          reference,
          half_life,
        ) < config.forget_below_frecency;
      let old_enough = newest.signed_duration_since(memory.created_at) > grace;
      let forget = faded && old_enough;
      verdicts.push(Verdict {
        memory_id: memory.id,
        version: memory.version,
        interest_score: score,
        forget,
      });
      if forget {
        forgotten.push(ForgottenMemory {
          memory_id: memory.id,
          text: memory.content.clone(),
        });
      }
    }
  })
  .await?;

  Ok(Some(Plan {
    report: Report {
      status: Status::DryRun,
      memories: rarity.memories(),
      interests: interest_reports,
      forgotten,
    },
    consolidation: Consolidation {
      interests,
      verdicts,
    },
  }))
}

/// Calls `visit` with every page of active memories, in order.
async fn each_page<S: Store + ?Sized>(
  store: &S,
  batch_size: usize,
  mut visit: impl FnMut(&[Embedded]),
) -> Result<(), StoreError> {
  let mut after = None;
  loop {
    let page = store.active(after, batch_size).await?;
    let Some(last) = page.last() else {
      return Ok(());
    };
    after = Some(last.memory.id);
    visit(&page);
    if page.len() < batch_size {
      return Ok(());
    }
  }
}

#[cfg(test)]
mod tests {
  use serde_json::Value;

  use super::*;
  use crate::fakes::FakeStore;
  use crate::store::{MemoryText, Write, WriteOutcome};
  use crate::types::{NewEvent, Vector, Version};

  const DAY: Duration = Duration::from_secs(60 * 60 * 24);
  const HALF_LIFE: Duration = DAY.saturating_mul(30);

  fn config() -> ConsolidationConfig {
    ConsolidationConfig {
      k_min: 2,
      k_max: 2,
      ..ConsolidationConfig::default()
    }
  }

  async fn add(store: &FakeStore, text: &str, vector: Vector) -> MemoryId {
    let write = Write::New {
      event: NewEvent {
        content: text.to_owned(),
        metadata: Value::Null,
      },
      memory: MemoryText {
        text: text.to_owned(),
        hash: text.to_owned(),
        vector,
        interest_score: None,
      },
    };
    match store.write(write).await.unwrap() {
      WriteOutcome::Written(id) => id,
      other => panic!("expected a write, got {other:?}"),
    }
  }

  /// Three memories about cats, three about dogs.
  async fn pets(store: &FakeStore) {
    for (text, vector) in [
      ("cat sofa", vec![1.0, 0.1, 0.0]),
      ("cat mouse", vec![1.0, -0.1, 0.0]),
      ("cat purr", vec![0.9, 0.0, 0.0]),
      ("dog bone", vec![0.0, 1.0, 0.1]),
      ("dog park", vec![0.1, 1.0, 0.0]),
      ("dog ball", vec![0.0, 0.9, -0.1]),
    ] {
      add(store, text, vector).await;
    }
  }

  async fn run(store: &FakeStore, mode: Mode) -> Report {
    consolidate(store, &config(), HALF_LIFE, mode)
      .await
      .unwrap()
  }

  fn applied() -> Mode {
    Mode::Apply {
      only_if_needed: None,
    }
  }

  fn first_terms(report: &Report) -> Vec<String> {
    let mut first: Vec<String> = report
      .interests
      .iter()
      .filter_map(|interest| interest.top_terms.first().cloned())
      .collect();
    first.sort();
    first
  }

  #[tokio::test]
  async fn the_interests_are_named_by_their_words() {
    let store = FakeStore::new();
    pets(&store).await;
    let report = run(&store, Mode::DryRun).await;
    assert_eq!(report.memories, 6);
    assert_eq!(first_terms(&report), ["cat", "dog"]);
    for interest in &report.interests {
      assert_eq!(interest.memories, 3);
      assert!(interest.nearest.is_some());
    }
  }

  #[tokio::test]
  async fn a_dry_run_writes_nothing() {
    let store = FakeStore::new();
    pets(&store).await;
    let report = run(&store, Mode::DryRun).await;
    assert_eq!(report.status, Status::DryRun);
    assert_eq!(store.consolidations().await, 0);
    assert!(store.interests().await.unwrap().is_empty());
  }

  #[tokio::test]
  async fn applying_writes_the_interests_and_scores() {
    let store = FakeStore::new();
    pets(&store).await;
    let report = run(&store, applied()).await;
    assert_eq!(report.status, Status::Written);
    assert_eq!(store.interests().await.unwrap().len(), 2);
    let memory = store.memory(MemoryId::new(1)).await.unwrap().unwrap();
    assert!(memory.interest_score.unwrap() > 0.9);
  }

  #[tokio::test]
  async fn there_is_nothing_to_consolidate_without_memories() {
    let store = FakeStore::new();
    let report = run(&store, applied()).await;
    assert_eq!(report.status, Status::NothingToConsolidate);
    assert_eq!(store.consolidations().await, 0);
  }

  #[tokio::test]
  async fn one_memory_makes_one_interest() {
    let store = FakeStore::new();
    add(&store, "cat", vec![1.0, 0.0, 0.0]).await;
    let report = run(&store, applied()).await;
    assert_eq!(report.interests.len(), 1);
    assert_eq!(report.status, Status::Written);
  }

  /// An old memory that nothing reinforced and nothing near it, among fresh
  /// ones.
  async fn pets_and_a_stray(store: &FakeStore) -> MemoryId {
    let stray = add(store, "tax form", vec![0.0, 0.0, 1.0]).await;
    store.age_activity(TimeDelta::days(100)).await;
    pets(store).await;
    stray
  }

  #[tokio::test]
  async fn an_old_unreinforced_stray_is_forgotten() {
    let store = FakeStore::new();
    let stray = pets_and_a_stray(&store).await;

    let report = run(&store, Mode::DryRun).await;
    let ids: Vec<MemoryId> = report
      .forgotten
      .iter()
      .map(|memory| memory.memory_id)
      .collect();
    assert_eq!(ids, [stray]);
    assert!(
      store
        .memory(stray)
        .await
        .unwrap()
        .unwrap()
        .forgotten_at
        .is_none()
    );

    run(&store, applied()).await;
    let forgotten = store.memory(stray).await.unwrap().unwrap();
    assert!(forgotten.forgotten_at.is_some());
  }

  #[tokio::test]
  async fn a_stray_inside_the_grace_period_is_kept() {
    let store = FakeStore::new();
    pets_and_a_stray(&store).await;
    let config = ConsolidationConfig {
      grace: DAY.saturating_mul(365),
      ..config()
    };
    let report = consolidate(&store, &config, HALF_LIFE, Mode::DryRun)
      .await
      .unwrap();
    assert!(report.forgotten.is_empty(), "{:?}", report.forgotten);
  }

  #[tokio::test]
  async fn a_reinforced_stray_is_kept() {
    let store = FakeStore::new();
    let stray = pets_and_a_stray(&store).await;
    for version in 1..=20 {
      let write = Write::Repeat {
        memory_id: stray,
        version: Version::new(version),
        event: NewEvent {
          content: "again".to_owned(),
          metadata: Value::Null,
        },
      };
      store.write(write).await.unwrap();
    }
    let report = run(&store, Mode::DryRun).await;
    assert!(report.forgotten.is_empty(), "{:?}", report.forgotten);
  }

  #[tokio::test]
  async fn a_second_run_finds_the_same_interests_and_forgets_nothing_more() {
    let store = FakeStore::new();
    let stray = pets_and_a_stray(&store).await;
    let first = run(&store, applied()).await;
    assert_eq!(first.forgotten.len(), 1);

    let second = run(&store, applied()).await;
    assert_eq!(first_terms(&second), first_terms(&first));
    assert!(second.forgotten.is_empty());
    let stray = store.memory(stray).await.unwrap().unwrap();
    assert!(stray.forgotten_at.is_some());
  }

  #[tokio::test]
  async fn a_long_quiet_spell_forgets_nothing() {
    let store = FakeStore::new();
    pets(&store).await;
    add(&store, "tax form", vec![0.0, 0.0, 1.0]).await;
    store.age_activity(TimeDelta::days(1_000)).await;
    let report = run(&store, Mode::DryRun).await;
    assert!(report.forgotten.is_empty(), "{:?}", report.forgotten);
  }

  #[tokio::test]
  async fn one_new_event_after_a_long_gap_does_not_age_the_rest_at_once() {
    let store = FakeStore::new();
    pets(&store).await;
    add(&store, "tax form", vec![0.0, 0.0, 1.0]).await;
    store.age_activity(TimeDelta::days(1_000)).await;
    add(&store, "bird song", vec![0.5, 0.5, 0.5]).await;
    let report = run(&store, Mode::DryRun).await;
    assert!(report.forgotten.is_empty(), "{:?}", report.forgotten);
  }

  #[tokio::test]
  async fn only_the_first_of_two_runs_writes_when_it_is_needed() {
    let store = FakeStore::new();
    pets(&store).await;
    store.age_activity(TimeDelta::days(30)).await;
    add(&store, "fresh", vec![0.0, 0.0, 1.0]).await;
    let needed = Mode::Apply {
      only_if_needed: Some(DAY.saturating_mul(14)),
    };

    assert_eq!(run(&store, needed).await.status, Status::Written);
    assert_eq!(run(&store, needed).await.status, Status::NotNeeded);
    assert_eq!(store.consolidations().await, 1);
  }

  #[tokio::test]
  async fn a_run_that_is_not_needed_writes_nothing() {
    let store = FakeStore::new();
    pets(&store).await;
    let needed = Mode::Apply {
      only_if_needed: Some(DAY.saturating_mul(14)),
    };
    assert_eq!(run(&store, needed).await.status, Status::NotNeeded);
    assert_eq!(store.consolidations().await, 0);
  }

  #[tokio::test]
  async fn pages_smaller_than_the_memories_give_the_same_report() {
    let store = FakeStore::new();
    pets_and_a_stray(&store).await;
    let whole = run(&store, Mode::DryRun).await;
    let small = ConsolidationConfig {
      batch_size: 2,
      ..config()
    };
    let paged = consolidate(&store, &small, HALF_LIFE, Mode::DryRun)
      .await
      .unwrap();
    assert_eq!(paged, whole);
  }
}
