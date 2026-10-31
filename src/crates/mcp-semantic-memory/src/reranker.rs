//! The rerank facade. The real one talks to a Jina-style rerank server.

use async_trait::async_trait;
use auto_impl::auto_impl;
use thiserror::Error;

use crate::types::BoxError;

/// Why the rerank server gave no scores.
#[derive(Debug, Error)]
pub enum RerankError {
  /// The request did not get an answer.
  #[error("could not ask the rerank server: {0}")]
  Failed(#[source] BoxError),
  /// The server answered with something that is not a list of scores.
  #[error("the rerank server sent an answer that makes no sense: {0}")]
  BadResponse(String),
}

/// Scores how well documents answer a query. A `Box<dyn Reranker>` is one
/// too, so the one in use can be picked while the program runs.
#[async_trait]
#[auto_impl(Box)]
pub trait Reranker: Send + Sync {
  /// One score per document, in the same order. Higher is better.
  async fn rerank(
    &self,
    query: &str,
    documents: &[String],
  ) -> Result<Vec<f32>, RerankError>;
}

/// Keeps the order it is given. Used when no rerank server is configured.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoReranker;

#[async_trait]
impl Reranker for NoReranker {
  async fn rerank(
    &self,
    _query: &str,
    documents: &[String],
  ) -> Result<Vec<f32>, RerankError> {
    Ok(
      std::iter::successors(Some(0.0_f32), |score| Some(score - 1.0))
        .take(documents.len())
        .collect(),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn no_reranker_scores_the_first_document_highest() {
    let documents = ["a", "b", "c"].map(str::to_owned);
    let scores = NoReranker.rerank("query", &documents).await.unwrap();
    assert_eq!(scores, [0.0, -1.0, -2.0]);
    assert!(NoReranker.rerank("query", &[]).await.unwrap().is_empty());
  }
}
