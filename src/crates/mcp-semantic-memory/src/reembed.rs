//! The run after the embedding model changed: every memory is embedded again
//! with the current model, in pages, into a table beside the live one, and
//! the tables are swapped once nothing is missing from the new one.

use std::fmt;

use thiserror::Error;

use crate::config::ReembedConfig;
use crate::embedder::{EmbedError, EmbedKind, Embedder};
use crate::store::{Store, StoreError, Swapped};

/// Why a reembed stopped. What was already embedded is kept, so running it
/// again picks up from there.
#[derive(Debug, Error)]
pub enum ReembedError {
  /// The embedding server failed.
  #[error("could not embed the memories: {0}")]
  Embed(#[from] EmbedError),
  /// The database failed.
  #[error(transparent)]
  Store(#[from] StoreError),
  /// The embedding server sent a different number of vectors than texts.
  #[error("the embedding server returned {got} vectors for {wanted} texts")]
  VectorCount {
    /// How many texts were sent.
    wanted: usize,
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
  /// The store says memories are missing from the new table, and also that
  /// there are none to embed.
  #[error("the store cannot swap the vectors and has nothing left to embed")]
  Stuck,
}

/// What a reembed did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
  /// How many memories were embedded by this run. A run that picked up an
  /// earlier one leaves out what the earlier one did.
  pub embedded: usize,
}

impl fmt::Display for Report {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    writeln!(
      formatter,
      "Embedded {} memories with the current model and swapped the vectors \
       in.",
      self.embedded
    )?;
    writeln!(
      formatter,
      "The interests have no vectors now. Run `consolidate --apply` to make \
       them again."
    )
  }
}

/// Embeds every memory, forgotten ones too, with `embedder`, and makes the
/// result the live vectors. It never holds more than a page. It can be
/// stopped at any point: the live vectors stay as they were until the swap,
/// and running it again continues with the memories that have no new vector.
/// Memories written while it runs are embedded before the swap. Stop every
/// instance that serves the memory first: a text changed after its page was
/// read would keep the vector of the old text.
///
/// The memory has to be told to consolidate afterwards, because the swap
/// drops the interest vectors.
pub async fn reembed<S, E>(
  store: &S,
  embedder: &E,
  config: &ReembedConfig,
) -> Result<Report, ReembedError>
where
  S: Store + ?Sized,
  E: Embedder + ?Sized,
{
  store.begin_reembed().await?;

  let mut embedded: usize = 0;
  let mut tried_to_swap = false;
  loop {
    let page = store.unembedded(config.batch_size).await?;
    if page.is_empty() {
      if tried_to_swap {
        return Err(ReembedError::Stuck);
      }
      match store.swap_vectors().await? {
        Swapped::Done => break,
        Swapped::Missing => {
          tried_to_swap = true;
          continue;
        }
      }
    }
    tried_to_swap = false;

    let texts: Vec<String> =
      page.iter().map(|memory| memory.content.clone()).collect();
    let vectors = embedder.embed(EmbedKind::Document, &texts).await?;
    if vectors.len() != texts.len() {
      return Err(ReembedError::VectorCount {
        wanted: texts.len(),
        got: vectors.len(),
      });
    }
    let expected = embedder.fingerprint().dimension;
    if let Some(vector) = vectors.iter().find(|vector| vector.len() != expected)
    {
      return Err(ReembedError::VectorLength {
        got: vector.len(),
        expected,
      });
    }

    embedded = embedded.saturating_add(texts.len());
    store
      .put_reembedded(
        page.iter().map(|memory| memory.id).zip(vectors).collect(),
      )
      .await?;
    tracing::info!(embedded, "embedded a page of memories");
  }
  Ok(Report { embedded })
}

#[cfg(test)]
mod tests {
  use async_trait::async_trait;

  use super::*;
  use crate::fakes::{FakeEmbedder, FakeStore};
  use crate::store::Forgotten;
  use crate::types::{Fingerprint, MemoryId, Vector};

  fn embedder() -> FakeEmbedder {
    FakeEmbedder::new(3)
      .with_word("cat", vec![1.0, 0.0, 0.0])
      .with_word("dog", vec![0.0, 1.0, 0.0])
      .with_word("tax", vec![0.0, 0.0, 1.0])
  }

  fn config(batch_size: usize) -> ReembedConfig {
    ReembedConfig { batch_size }
  }

  /// A store whose vectors were all made by another model: they all point
  /// along the first axis.
  async fn store_of_another_model() -> (FakeStore, [MemoryId; 3]) {
    let store = FakeStore::new();
    let cat = store.add("cat sleeps", vec![1.0, 0.0, 0.0]).await;
    let dog = store.add("dog barks", vec![1.0, 0.0, 0.0]).await;
    let tax = store.add("tax is due", vec![1.0, 0.0, 0.0]).await;
    (store, [cat, dog, tax])
  }

  async fn nearest_to(store: &FakeStore, vector: &[f32]) -> MemoryId {
    let found = store.nearest(vector, 1, Forgotten::Include).await.unwrap();
    found[0].memory.id
  }

  #[tokio::test]
  async fn every_memory_gets_a_vector_from_the_current_model() {
    let (store, [_, dog, tax]) = store_of_another_model().await;

    let report = reembed(&store, &embedder(), &config(2)).await.unwrap();

    assert_eq!(report.embedded, 3);
    assert_eq!(nearest_to(&store, &[0.0, 1.0, 0.0]).await, dog);
    assert_eq!(nearest_to(&store, &[0.0, 0.0, 1.0]).await, tax);
  }

  #[tokio::test]
  async fn forgotten_memories_are_embedded_too() {
    let (store, [_, dog, _]) = store_of_another_model().await;
    store.forget(dog).await;

    reembed(&store, &embedder(), &config(10)).await.unwrap();

    assert_eq!(nearest_to(&store, &[0.0, 1.0, 0.0]).await, dog);
  }

  #[tokio::test]
  async fn the_interests_are_dropped_by_the_swap() {
    let (store, _) = store_of_another_model().await;
    store.set_interests(vec![vec![1.0, 0.0, 0.0]]).await;

    reembed(&store, &embedder(), &config(10)).await.unwrap();

    assert!(store.interests().await.unwrap().is_empty());
  }

  #[tokio::test]
  async fn an_earlier_run_is_picked_up_where_it_stopped() {
    let (store, [cat, ..]) = store_of_another_model().await;
    store.begin_reembed().await.unwrap();
    store
      .put_reembedded(vec![(cat, vec![1.0, 0.0, 0.0])])
      .await
      .unwrap();

    let report = reembed(&store, &embedder(), &config(10)).await.unwrap();

    assert_eq!(report.embedded, 2);
    assert_eq!(store.health().await.unwrap().memories_to_reembed, 0);
  }

  /// Says its vectors have three numbers and sends two.
  struct ShortVectors {
    fingerprint: Fingerprint,
  }

  #[async_trait]
  impl Embedder for ShortVectors {
    async fn embed(
      &self,
      _kind: EmbedKind,
      texts: &[String],
    ) -> Result<Vec<Vector>, EmbedError> {
      Ok(texts.iter().map(|_| vec![0.0, 1.0]).collect())
    }

    fn fingerprint(&self) -> &Fingerprint {
      &self.fingerprint
    }
  }

  #[tokio::test]
  async fn nothing_is_swapped_when_the_server_sends_the_wrong_length() {
    let (store, _) = store_of_another_model().await;
    store.set_interests(vec![vec![1.0, 0.0, 0.0]]).await;
    let short = ShortVectors {
      fingerprint: Fingerprint {
        model: "short".to_owned(),
        dimension: 3,
      },
    };

    let error = reembed(&store, &short, &config(10)).await.unwrap_err();

    assert!(
      matches!(error, ReembedError::VectorLength { .. }),
      "{error}"
    );
    assert_eq!(store.interests().await.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn a_store_with_no_memories_swaps_at_once() {
    let store = FakeStore::new();
    let report = reembed(&store, &embedder(), &config(10)).await.unwrap();
    assert_eq!(report.embedded, 0);
  }

  #[test]
  fn the_report_says_what_to_do_next() {
    let text = Report { embedded: 4 }.to_string();
    assert!(text.contains("4 memories"), "{text}");
    assert!(text.contains("`consolidate"), "{text}");
  }
}
