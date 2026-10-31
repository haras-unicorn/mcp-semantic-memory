//! The embedding facade. The real one talks to an OpenAI-compatible server.

use async_trait::async_trait;
use auto_impl::auto_impl;
use thiserror::Error;

use crate::types::{BoxError, Fingerprint, Vector};

/// Why the embedding server gave no vectors.
#[derive(Debug, Error)]
pub enum EmbedError {
  /// The request did not get an answer.
  #[error("could not ask the embedding server: {0}")]
  Failed(#[source] BoxError),
  /// The server answered with something that is not a list of vectors.
  #[error("the embedding server sent an answer that makes no sense: {0}")]
  BadResponse(String),
}

/// Many embedding models expect different text in front of a query and of a
/// document, so the caller says which one it is embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
  /// Text to search with.
  Query,
  /// Text to be found.
  Document,
}

/// Turns text into vectors. A `Box<dyn Embedder>` is one too, so the one in
/// use can be picked while the program runs.
#[async_trait]
#[auto_impl(Box)]
pub trait Embedder: Send + Sync {
  /// One vector per text, in the same order.
  async fn embed(
    &self,
    kind: EmbedKind,
    texts: &[String],
  ) -> Result<Vec<Vector>, EmbedError>;

  /// What the vectors from this embedder are made of. A store keeps it with
  /// its vectors, so it can tell when the model changed.
  fn fingerprint(&self) -> &Fingerprint;
}
