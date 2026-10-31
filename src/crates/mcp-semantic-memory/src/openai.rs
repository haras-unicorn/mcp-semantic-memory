//! The embedder for any server with an OpenAI-compatible `/v1/embeddings`
//! endpoint, like the llama.cpp server, vLLM or Ollama.
//!
//! With llama.cpp, start the server with the embedding model and a pooling
//! other than `none`, for example
//! `llama-server -m model.gguf --embedding --pooling mean`. It answers with
//! vectors that are already normalized.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};

use crate::config::EmbeddingConfig;
use crate::embedder::{EmbedError, EmbedKind, Embedder};
use crate::http;
use crate::types::{Fingerprint, Vector};

const PATH: &str = "v1/embeddings";

#[derive(Serialize)]
struct Request<'a> {
  input: &'a [String],
  encoding_format: &'static str,
}

#[derive(Deserialize)]
struct Response {
  data: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
  index: usize,
  embedding: Vector,
}

/// Embeds text with an OpenAI-compatible server.
#[derive(Debug)]
pub struct OpenAiEmbedder {
  client: Client,
  endpoint: Url,
  query_prefix: String,
  document_prefix: String,
  fingerprint: Fingerprint,
}

impl OpenAiEmbedder {
  /// Fails when `config.url` is not an address. The server is not asked
  /// anything yet.
  pub fn new(
    config: &EmbeddingConfig,
    timeout: Duration,
  ) -> Result<Self, EmbedError> {
    Ok(Self {
      client: http::client(timeout).map_err(EmbedError::Failed)?,
      endpoint: http::endpoint(&config.url, PATH)
        .map_err(EmbedError::Failed)?,
      query_prefix: config.query_prefix.clone(),
      document_prefix: config.document_prefix.clone(),
      fingerprint: Fingerprint {
        model: config.model.clone(),
        dimension: config.dimension,
      },
    })
  }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
  async fn embed(
    &self,
    kind: EmbedKind,
    texts: &[String],
  ) -> Result<Vec<Vector>, EmbedError> {
    if texts.is_empty() {
      return Ok(Vec::new());
    }

    let prefix = match kind {
      EmbedKind::Query => &self.query_prefix,
      EmbedKind::Document => &self.document_prefix,
    };
    let input = prefixed(prefix, texts);
    let request = self.client.post(self.endpoint.clone()).json(&Request {
      input: &input,
      encoding_format: "float",
    });

    let response = http::send(request).await.map_err(EmbedError::Failed)?;
    let response: Response = response
      .json()
      .await
      .map_err(|error| EmbedError::BadResponse(error.to_string()))?;
    vectors(response, texts.len())
  }

  fn fingerprint(&self) -> &Fingerprint {
    &self.fingerprint
  }
}

fn prefixed(prefix: &str, texts: &[String]) -> Vec<String> {
  texts.iter().map(|text| format!("{prefix}{text}")).collect()
}

/// The vectors in the order of the texts. The server numbers them, and does
/// not promise to send them in order.
fn vectors(
  response: Response,
  wanted: usize,
) -> Result<Vec<Vector>, EmbedError> {
  let mut items = response.data;
  if items.len() != wanted {
    return Err(EmbedError::BadResponse(format!(
      "{} vectors for {wanted} texts",
      items.len()
    )));
  }
  items.sort_by_key(|item| item.index);
  if items
    .iter()
    .enumerate()
    .any(|(position, item)| item.index != position)
  {
    return Err(EmbedError::BadResponse(
      "the vectors are not numbered from 0 without gaps".to_owned(),
    ));
  }
  Ok(items.into_iter().map(|item| item.embedding).collect())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn response(json: &str) -> Response {
    serde_json::from_str(json).unwrap()
  }

  #[test]
  fn the_answer_of_an_openai_compatible_server_is_read() {
    let answer = response(
      r#"{
        "model": "nomic",
        "object": "list",
        "usage": {"prompt_tokens": 3, "total_tokens": 3},
        "data": [
          {"object": "embedding", "index": 0, "embedding": [0.5, -0.5]}
        ]
      }"#,
    );
    assert_eq!(vectors(answer, 1).unwrap(), [[0.5, -0.5]]);
  }

  #[test]
  fn vectors_come_back_in_the_order_of_the_texts() {
    let answer = response(
      r#"{"data": [
        {"index": 1, "embedding": [2.0]},
        {"index": 0, "embedding": [1.0]}
      ]}"#,
    );
    assert_eq!(vectors(answer, 2).unwrap(), [[1.0], [2.0]]);
  }

  #[test]
  fn a_different_number_of_vectors_is_a_bad_response() {
    let answer = response(r#"{"data": [{"index": 0, "embedding": [1.0]}]}"#);
    let error = vectors(answer, 2).unwrap_err();
    assert!(matches!(error, EmbedError::BadResponse(_)), "{error}");
  }

  #[test]
  fn vectors_numbered_with_a_gap_are_a_bad_response() {
    let answer = response(
      r#"{"data": [
        {"index": 0, "embedding": [1.0]},
        {"index": 2, "embedding": [2.0]}
      ]}"#,
    );
    let error = vectors(answer, 2).unwrap_err();
    assert!(matches!(error, EmbedError::BadResponse(_)), "{error}");
  }

  #[test]
  fn the_prefix_goes_in_front_of_every_text() {
    let texts = ["one", "two"].map(str::to_owned);
    assert_eq!(prefixed("search: ", &texts), ["search: one", "search: two"]);
    assert_eq!(prefixed("", &texts), ["one", "two"]);
  }

  #[test]
  fn the_fingerprint_comes_from_the_configuration() {
    let config = EmbeddingConfig {
      url: "http://localhost:8080".to_owned(),
      model: "nomic".to_owned(),
      dimension: 768,
      ..EmbeddingConfig::default()
    };
    let embedder =
      OpenAiEmbedder::new(&config, Duration::from_secs(1)).unwrap();
    assert_eq!(embedder.fingerprint().to_string(), "nomic/768/cosine");
  }

  #[test]
  fn a_url_that_is_not_an_address_is_refused() {
    let config = EmbeddingConfig {
      url: "not an address".to_owned(),
      ..EmbeddingConfig::default()
    };
    let error =
      OpenAiEmbedder::new(&config, Duration::from_secs(1)).unwrap_err();
    assert!(matches!(error, EmbedError::Failed(_)), "{error}");
  }
}
