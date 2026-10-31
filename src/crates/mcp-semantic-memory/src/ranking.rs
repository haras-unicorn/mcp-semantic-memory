//! The scoring rules of search. Pure functions: no I/O, no clock of their own.
//! Recency is always measured from a reference time that the caller works out
//! from the data, never from the wall clock.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::config::SearchConfig;
use crate::types::{Candidate, Memory, MemoryId, SearchHit, Vector};
use crate::vector::cosine_distance;

#[allow(
  clippy::as_conversions,
  reason = "i64 has no lossless conversion to f32, and counts and ages are \
            far below the 2^24 where f32 stops being exact"
)]
pub(crate) fn float(number: i64) -> f32 {
  number as f32
}

/// A count as a float, with the same limits as `float`.
pub(crate) fn count_as_float(count: usize) -> f32 {
  float(i64::try_from(count).unwrap_or(i64::MAX))
}

/// The median of some times, or nothing when there are none. For an even
/// number it is the point halfway between the two in the middle. It is the
/// reference that recency is measured from: a few very old or very new
/// memories barely move it, and a quiet spell does not move it at all.
///
/// ```
/// use chrono::{TimeDelta, Utc};
/// use mcp_semantic_memory::ranking::median_time;
///
/// let now = Utc::now();
/// let times = [now, now - TimeDelta::days(10), now - TimeDelta::days(2)];
/// let median = median_time(&times).unwrap();
/// let expected = now - TimeDelta::days(2);
/// assert_eq!(median.timestamp_millis(), expected.timestamp_millis());
/// assert_eq!(median_time(&[]), None);
/// ```
pub fn median_time(times: &[DateTime<Utc>]) -> Option<DateTime<Utc>> {
  let mut millis: Vec<i64> =
    times.iter().map(DateTime::timestamp_millis).collect();
  millis.sort_unstable();
  let middle = millis.len() / 2;
  let upper = *millis.get(middle)?;
  let median = match middle.checked_sub(1).and_then(|lower| millis.get(lower)) {
    Some(lower) if millis.len().is_multiple_of(2) => lower.midpoint(upper),
    _ => upper,
  };
  DateTime::from_timestamp_millis(median)
}

/// How much a memory has been reinforced lately: the log of its event count,
/// halved for every half-life between its last event and the `reference`
/// time. A memory newer than the reference counts as fresh. Only writes change
/// it. The half-life must be more than zero, which the configuration checks.
///
/// ```
/// use std::time::Duration;
///
/// use chrono::{TimeDelta, Utc};
/// use mcp_semantic_memory::ranking::frecency;
///
/// let reference = Utc::now();
/// let day = Duration::from_secs(24 * 60 * 60);
/// let fresh = frecency(3, reference, reference, day);
/// let a_day_old = frecency(3, reference - TimeDelta::days(1), reference, day);
/// assert!((a_day_old - fresh / 2.0).abs() < 1e-4);
/// ```
pub fn frecency(
  event_count: i64,
  last_event_at: DateTime<Utc>,
  reference: DateTime<Utc>,
  half_life: Duration,
) -> f32 {
  let age = float(
    reference
      .signed_duration_since(last_event_at)
      .num_seconds()
      .max(0),
  );
  (1.0 + float(event_count.max(0))).ln()
    * 0.5_f32.powf(age / half_life.as_secs_f32())
}

/// The best cosine similarity between a vector and any interest centre. There
/// is no score before the first consolidation.
///
/// ```
/// use mcp_semantic_memory::ranking::interest_score;
///
/// let centres = vec![vec![0.0, 1.0], vec![1.0, 0.0]];
/// let score = interest_score(&[2.0, 0.0], &centres).unwrap();
/// assert!((score - 1.0).abs() < 1e-6);
/// assert_eq!(interest_score(&[2.0, 0.0], &[]), None);
/// ```
pub fn interest_score(vector: &[f32], interests: &[Vector]) -> Option<f32> {
  interests
    .iter()
    .map(|centre| 1.0 - cosine_distance(vector, centre))
    .max_by(f32::total_cmp)
}

/// How many memories each word is in. A word in every memory says nothing
/// about any one cluster, so it scores zero, in any language, with no list of
/// filler words.
#[derive(Debug, Default)]
pub struct Rarity {
  memories: usize,
  containing: HashMap<String, usize>,
}

impl Rarity {
  /// How many memories were counted.
  pub fn memories(&self) -> usize {
    self.memories
  }

  /// Counts the words of one more memory.
  pub fn add(&mut self, text: &str) {
    self.memories = self.memories.saturating_add(1);
    for word in keywords(text) {
      let count = self.containing.entry(word).or_insert(0);
      *count = count.saturating_add(1);
    }
  }

  fn of(&self, word: &str) -> f32 {
    let containing = self.containing.get(word).copied().unwrap_or(1);
    (count_as_float(self.memories) / count_as_float(containing)).ln()
  }
}

/// The words that best name a cluster, best first: each word scores the
/// weight of the memories it is in, times how rare it is among all memories.
/// Words that score nothing are left out.
///
/// ```
/// use mcp_semantic_memory::ranking::{Rarity, top_terms};
///
/// let texts = [
///   "cat sleeps",
///   "cat sleeps today",
///   "dog barks",
///   "dog runs",
///   "bird sings",
/// ];
/// let mut rarity = Rarity::default();
/// for text in texts {
///   rarity.add(text);
/// }
/// let cluster = [("cat sleeps", 1.0), ("cat sleeps today", 1.0)];
/// assert_eq!(top_terms(&cluster, &rarity, 2), ["cat", "sleeps"]);
/// ```
pub fn top_terms(
  members: &[(&str, f32)],
  rarity: &Rarity,
  count: usize,
) -> Vec<String> {
  let mut weighted: HashMap<String, f32> = HashMap::new();
  for (text, weight) in members {
    for word in keywords(text) {
      *weighted.entry(word).or_insert(0.0) += weight.max(0.0);
    }
  }
  let mut scored: Vec<(String, f32)> = weighted
    .into_iter()
    .map(|(word, weight)| {
      let score = weight * rarity.of(&word);
      (word, score)
    })
    .filter(|(_, score)| *score > 0.0)
    .collect();
  scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
  scored
    .into_iter()
    .take(count)
    .map(|(word, _)| word)
    .collect()
}

/// The distinct words of a text, lowercased and in order. The stores turn
/// these into a query of their own, so nothing in the text can be taken for
/// query syntax.
///
/// ```
/// use mcp_semantic_memory::ranking::keywords;
///
/// assert_eq!(keywords("Cat, cat! sat?"), ["cat", "sat"]);
/// assert!(keywords("  ... \" ' ( ) ").is_empty());
/// ```
pub fn keywords(text: &str) -> Vec<String> {
  let mut words: Vec<String> = text
    .split(|c: char| !c.is_alphanumeric())
    .filter(|word| !word.is_empty())
    .map(str::to_lowercase)
    .collect();
  words.sort();
  words.dedup();
  words
}

/// Merges the vector and keyword rankings by weighted reciprocal rank: a
/// memory gets `weight / (constant + rank)` points from each list it is in.
/// Frecency and the interest score then lift the result a little. The lift is
/// a factor, so it does not depend on the scale of the rank points. Recency
/// is measured from the median last event of the memories found. Best first.
pub fn fuse(
  vector_hits: Vec<Candidate>,
  keyword_hits: Vec<Memory>,
  config: &SearchConfig,
) -> Vec<SearchHit> {
  let lists = [
    (
      config.vector_weight,
      vector_hits
        .into_iter()
        .map(|hit| hit.memory)
        .collect::<Vec<Memory>>(),
    ),
    (config.keyword_weight, keyword_hits),
  ];

  let mut found: HashMap<MemoryId, SearchHit> = HashMap::new();
  for (weight, memories) in lists {
    for (index, memory) in memories.into_iter().enumerate() {
      let rank = float(i64::try_from(index).unwrap_or(i64::MAX));
      let points = weight / (config.rank_constant + rank + 1.0);
      found
        .entry(memory.id)
        .and_modify(|hit| hit.score += points)
        .or_insert(SearchHit {
          memory,
          score: points,
        });
    }
  }

  let times: Vec<DateTime<Utc>> =
    found.values().map(|hit| hit.memory.last_event_at).collect();
  let Some(reference) = median_time(&times) else {
    return Vec::new();
  };
  let mut hits: Vec<SearchHit> = found
    .into_values()
    .map(|mut hit| {
      hit.score *= lift(&hit.memory, config, reference);
      hit
    })
    .collect();
  hits.sort_by(|a, b| {
    b.score
      .total_cmp(&a.score)
      .then(a.memory.id.cmp(&b.memory.id))
  });
  hits
}

fn lift(
  memory: &Memory,
  config: &SearchConfig,
  reference: DateTime<Utc>,
) -> f32 {
  let frecency = frecency(
    memory.event_count,
    memory.last_event_at,
    reference,
    config.half_life,
  );
  let interest = memory.interest_score.unwrap_or(0.0).clamp(0.0, 1.0);
  1.0 + config.frecency_weight * frecency + config.interest_weight * interest
}

#[cfg(test)]
mod tests {
  use chrono::TimeDelta;

  use super::*;
  use crate::types::Version;

  const DAY: Duration = Duration::from_secs(24 * 60 * 60);

  fn config() -> SearchConfig {
    SearchConfig {
      frecency_weight: 0.0,
      interest_weight: 0.0,
      half_life: DAY,
      ..SearchConfig::default()
    }
  }

  fn memory(id: i64, now: DateTime<Utc>) -> Memory {
    Memory {
      id: MemoryId::new(id),
      content: format!("memory {id}"),
      content_hash: format!("hash {id}"),
      event_count: 1,
      last_event_at: now,
      interest_score: None,
      forgotten_at: None,
      version: Version::new(1),
      created_at: now,
      updated_at: now,
    }
  }

  fn candidate(memory: Memory) -> Candidate {
    Candidate {
      memory,
      distance: 0.0,
    }
  }

  fn ids(hits: &[SearchHit]) -> Vec<i64> {
    hits.iter().map(|hit| hit.memory.id.get()).collect()
  }

  #[test]
  fn a_memory_in_both_lists_beats_one_in_a_single_list() {
    let now = Utc::now();
    let hits = fuse(
      vec![candidate(memory(1, now)), candidate(memory(2, now))],
      vec![memory(2, now)],
      &config(),
    );
    assert_eq!(ids(&hits), [2, 1]);
  }

  #[test]
  fn the_vector_list_counts_more_than_the_keyword_list() {
    let now = Utc::now();
    let hits = fuse(
      vec![candidate(memory(1, now))],
      vec![memory(2, now)],
      &config(),
    );
    assert_eq!(ids(&hits), [1, 2]);
  }

  #[test]
  fn frecency_halves_every_half_life() {
    let now = Utc::now();
    let fresh = frecency(3, now, now, DAY);
    let old = frecency(3, now - TimeDelta::days(1), now, DAY);
    assert!((old - fresh / 2.0).abs() < 1e-4);
  }

  #[test]
  fn frecency_grows_with_the_event_count() {
    let now = Utc::now();
    assert!(frecency(5, now, now, DAY) > frecency(1, now, now, DAY));
  }

  #[test]
  fn reinforced_memories_win_a_tie() {
    let now = Utc::now();
    let mut hot = memory(2, now);
    hot.event_count = 20;
    let config = SearchConfig {
      frecency_weight: 0.1,
      ..config()
    };
    let hits = fuse(
      vec![candidate(memory(1, now)), candidate(hot)],
      vec![],
      &config,
    );
    assert_eq!(ids(&hits), [2, 1]);
  }

  #[test]
  fn the_interest_score_lifts_a_memory() {
    let now = Utc::now();
    let mut keen = memory(2, now);
    keen.interest_score = Some(1.0);
    let config = SearchConfig {
      interest_weight: 0.1,
      ..config()
    };
    let hits = fuse(
      vec![candidate(memory(1, now)), candidate(keen)],
      vec![],
      &config,
    );
    assert_eq!(ids(&hits), [2, 1]);
  }

  #[test]
  fn the_interest_score_is_the_best_match_among_centres() {
    let centres = vec![vec![0.0, 1.0], vec![1.0, 0.0]];
    let score = interest_score(&[2.0, 0.0], &centres).unwrap();
    assert!((score - 1.0).abs() < 1e-6);
  }

  #[test]
  fn there_is_no_interest_score_without_interests() {
    assert_eq!(interest_score(&[1.0, 0.0], &[]), None);
  }

  #[test]
  fn the_median_is_the_middle_time_or_halfway_between_two() {
    let now = Utc::now();
    let day = |days| now - TimeDelta::days(days);
    let millis = |times: &[DateTime<Utc>]| {
      median_time(times).map(|time| time.timestamp_millis())
    };
    let expected = |time: DateTime<Utc>| Some(time.timestamp_millis());
    assert_eq!(millis(&[day(1)]), expected(day(1)));
    assert_eq!(millis(&[day(9), day(1), day(3)]), expected(day(3)));
    assert_eq!(millis(&[day(4), day(2)]), expected(day(3)));
  }

  #[test]
  fn a_memory_newer_than_the_reference_counts_as_fresh() {
    let reference = Utc::now();
    let newer = frecency(3, reference + TimeDelta::days(5), reference, DAY);
    assert_eq!(newer, frecency(3, reference, reference, DAY));
  }

  #[test]
  fn the_same_memories_rank_the_same_whenever_they_are_searched() {
    let long_ago = Utc::now() - TimeDelta::days(400);
    let config = SearchConfig {
      frecency_weight: 0.5,
      ..config()
    };
    let found = |now| {
      let mut hot = memory(2, now);
      hot.event_count = 9;
      let hits = fuse(
        vec![candidate(memory(1, now)), candidate(hot)],
        vec![],
        &config,
      );
      hits.into_iter().map(|hit| hit.score).collect::<Vec<f32>>()
    };
    assert_eq!(found(Utc::now()), found(long_ago));
  }

  #[test]
  fn an_old_memory_among_fresh_ones_is_told_apart_by_its_age() {
    let now = Utc::now();
    let config = SearchConfig {
      frecency_weight: 1.0,
      ..config()
    };
    let mut old = memory(1, now - TimeDelta::days(30));
    old.event_count = 4;
    let mut fresh = memory(2, now);
    fresh.event_count = 4;
    let hits = fuse(
      vec![candidate(old), candidate(fresh), candidate(memory(3, now))],
      vec![],
      &config,
    );
    let score = |id| {
      hits
        .iter()
        .find(|hit| hit.memory.id == MemoryId::new(id))
        .map(|hit| hit.score)
        .unwrap()
    };
    assert!(score(2) > score(1));
  }

  #[test]
  fn the_terms_of_a_cluster_are_its_rare_shared_words() {
    let texts = [
      "the cat sleeps on the sofa",
      "the cat chases a mouse",
      "the stock market fell today",
      "the bond market rose today",
      "the weather is warm",
      "an evening walk",
    ];
    let mut rarity = Rarity::default();
    for text in texts {
      rarity.add(text);
    }
    let cats = [(texts[0], 1.0), (texts[1], 1.0)];
    let terms = top_terms(&cats, &rarity, 2);
    assert_eq!(terms[0], "cat");
    assert!(!terms.contains(&"the".to_owned()), "{terms:?}");
  }

  #[test]
  fn a_word_in_every_memory_scores_nothing() {
    let mut rarity = Rarity::default();
    rarity.add("common alpha");
    rarity.add("common beta");
    let terms = top_terms(&[("common alpha", 1.0)], &rarity, 5);
    assert_eq!(terms, ["alpha"]);
  }

  #[test]
  fn heavier_memories_decide_the_terms() {
    let mut rarity = Rarity::default();
    for text in ["red apple", "green pear", "blue plum", "grey stone"] {
      rarity.add(text);
    }
    let members = [("red apple", 5.0), ("green pear", 1.0)];
    assert_eq!(top_terms(&members, &rarity, 1), ["apple"]);
  }

  #[test]
  fn the_terms_come_in_a_fixed_order() {
    let mut rarity = Rarity::default();
    for text in ["bravo alpha", "charlie delta", "echo"] {
      rarity.add(text);
    }
    let members = [("bravo alpha", 1.0)];
    assert_eq!(top_terms(&members, &rarity, 5), ["alpha", "bravo"]);
  }
}
