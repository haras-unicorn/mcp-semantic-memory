//! The reranker for any server with a Jina-style `/v1/rerank` endpoint: it
//! takes a `query` and `documents`, and answers with a `results` list of
//! `index` and `relevance_score`, best first. Jina, Cohere and the llama.cpp
//! server speak it. OpenAI has no rerank endpoint.
//!
//! With llama.cpp, start the server with a reranker GGUF and `--reranking`,
//! for example `llama-server -m reranker.gguf --reranking`. It is a different
//! server from the embedding one, since a server runs one model. Its scores
//! are probabilities between 0 and 1.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};

use crate::http;
use crate::reranker::{RerankError, Reranker};

const PATH: &str = "v1/rerank";

#[derive(Serialize)]
struct Request<'a> {
  query: &'a str,
  documents: &'a [String],
}

#[derive(Deserialize)]
struct Response {
  results: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
  index: usize,
  relevance_score: f32,
}

/// Scores documents with a Jina-style rerank server.
#[derive(Debug)]
pub struct JinaReranker {
  client: Client,
  endpoint: Url,
}

impl JinaReranker {
  /// Fails when `url` is not an address. The server is not asked anything
  /// yet.
  pub fn new(url: &str, timeout: Duration) -> Result<Self, RerankError> {
    Ok(Self {
      client: http::client(timeout).map_err(RerankError::Failed)?,
      endpoint: http::endpoint(url, PATH).map_err(RerankError::Failed)?,
    })
  }
}

#[async_trait]
impl Reranker for JinaReranker {
  async fn rerank(
    &self,
    query: &str,
    documents: &[String],
  ) -> Result<Vec<f32>, RerankError> {
    if documents.is_empty() {
      return Ok(Vec::new());
    }

    let request = self
      .client
      .post(self.endpoint.clone())
      .json(&Request { query, documents });

    let response = http::send(request).await.map_err(RerankError::Failed)?;
    let response: Response = response
      .json()
      .await
      .map_err(|error| RerankError::BadResponse(error.to_string()))?;
    scores(response, documents.len())
  }
}

/// The scores in the order of the documents. The server sends them best
/// first and says which document each one is for.
fn scores(response: Response, wanted: usize) -> Result<Vec<f32>, RerankError> {
  let mut items = response.results;
  if items.len() != wanted {
    return Err(RerankError::BadResponse(format!(
      "{} scores for {wanted} documents",
      items.len()
    )));
  }
  items.sort_by_key(|item| item.index);
  if items
    .iter()
    .enumerate()
    .any(|(position, item)| item.index != position)
  {
    return Err(RerankError::BadResponse(
      "the scores are not numbered from 0 without gaps".to_owned(),
    ));
  }
  Ok(items.into_iter().map(|item| item.relevance_score).collect())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn response(json: &str) -> Response {
    serde_json::from_str(json).unwrap()
  }

  #[test]
  fn the_answer_of_a_jina_style_server_is_read() {
    let answer = response(
      r#"{
        "model": "reranker",
        "object": "list",
        "usage": {"prompt_tokens": 9, "total_tokens": 9},
        "results": [
          {"index": 0, "relevance_score": 0.75}
        ]
      }"#,
    );
    assert_eq!(scores(answer, 1).unwrap(), [0.75]);
  }

  #[test]
  fn scores_follow_the_order_of_the_documents_and_not_the_ranking() {
    let answer = response(
      r#"{"results": [
        {"index": 1, "relevance_score": 0.9},
        {"index": 2, "relevance_score": 0.5},
        {"index": 0, "relevance_score": 0.1}
      ]}"#,
    );
    assert_eq!(scores(answer, 3).unwrap(), [0.1, 0.9, 0.5]);
  }

  #[test]
  fn a_different_number_of_scores_is_a_bad_response() {
    let answer =
      response(r#"{"results": [{"index": 0, "relevance_score": 1}]}"#);
    let error = scores(answer, 2).unwrap_err();
    assert!(matches!(error, RerankError::BadResponse(_)), "{error}");
  }

  #[test]
  fn scores_numbered_with_a_gap_are_a_bad_response() {
    let answer = response(
      r#"{"results": [
        {"index": 0, "relevance_score": 1},
        {"index": 2, "relevance_score": 0}
      ]}"#,
    );
    let error = scores(answer, 2).unwrap_err();
    assert!(matches!(error, RerankError::BadResponse(_)), "{error}");
  }

  #[test]
  fn a_url_that_is_not_an_address_is_refused() {
    let error =
      JinaReranker::new("not an address", Duration::from_secs(1)).unwrap_err();
    assert!(matches!(error, RerankError::Failed(_)), "{error}");
  }
}
