//! The HTTP facades against a mock server that answers the way
//! `llama-server` does.

#![cfg(test)]

use std::time::Duration;

use mcp_semantic_memory::config::EmbeddingConfig;
use mcp_semantic_memory::prelude::*;
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TIMEOUT: Duration = Duration::from_secs(5);

fn embedder(server: &MockServer) -> OpenAiEmbedder {
  let config = EmbeddingConfig {
    url: server.uri(),
    model: "mock".to_owned(),
    dimension: 2,
    query_prefix: "search: ".to_owned(),
    document_prefix: "document: ".to_owned(),
  };
  OpenAiEmbedder::new(&config, TIMEOUT).unwrap()
}

fn reranker(server: &MockServer) -> JinaReranker {
  JinaReranker::new(&server.uri(), TIMEOUT).unwrap()
}

fn texts(texts: &[&str]) -> Vec<String> {
  texts.iter().map(|text| (*text).to_owned()).collect()
}

#[tokio::test]
async fn a_query_is_sent_with_its_prefix_and_the_vectors_come_back_in_order() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/v1/embeddings"))
    .and(body_json(json!({
      "input": ["search: one", "search: two"],
      "encoding_format": "float",
    })))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "object": "list",
      "model": "mock",
      "data": [
        {"object": "embedding", "index": 1, "embedding": [0.0, 1.0]},
        {"object": "embedding", "index": 0, "embedding": [1.0, 0.0]},
      ],
      "usage": {"prompt_tokens": 4, "total_tokens": 4},
    })))
    .expect(1)
    .mount(&server)
    .await;

  let vectors = embedder(&server)
    .embed(EmbedKind::Query, &texts(&["one", "two"]))
    .await
    .unwrap();

  assert_eq!(vectors, [[1.0, 0.0], [0.0, 1.0]]);
}

#[tokio::test]
async fn a_document_is_sent_with_its_own_prefix() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/v1/embeddings"))
    .and(body_json(json!({
      "input": ["document: one"],
      "encoding_format": "float",
    })))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "data": [{"index": 0, "embedding": [1.0, 0.0]}],
    })))
    .expect(1)
    .mount(&server)
    .await;

  let vectors = embedder(&server)
    .embed(EmbedKind::Document, &texts(&["one"]))
    .await
    .unwrap();

  assert_eq!(vectors, [[1.0, 0.0]]);
}

#[tokio::test]
async fn no_texts_means_no_request() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .respond_with(ResponseTemplate::new(500))
    .expect(0)
    .mount(&server)
    .await;

  let vectors = embedder(&server)
    .embed(EmbedKind::Document, &[])
    .await
    .unwrap();

  assert!(vectors.is_empty());
}

#[tokio::test]
async fn the_words_of_a_server_that_refuses_are_kept() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .respond_with(
      ResponseTemplate::new(400)
        .set_body_json(json!({"error": {"message": "input is too long"}})),
    )
    .mount(&server)
    .await;

  let error = embedder(&server)
    .embed(EmbedKind::Document, &texts(&["one"]))
    .await
    .unwrap_err();

  assert!(matches!(error, EmbedError::Failed(_)), "{error}");
  assert!(error.to_string().contains("input is too long"), "{error}");
  assert!(error.to_string().contains("400"), "{error}");
}

#[tokio::test]
async fn an_answer_that_is_not_vectors_is_a_bad_response() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
    .mount(&server)
    .await;

  let error = embedder(&server)
    .embed(EmbedKind::Document, &texts(&["one"]))
    .await
    .unwrap_err();

  assert!(matches!(error, EmbedError::BadResponse(_)), "{error}");
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_failure() {
  let server = MockServer::start().await;
  let embedder = embedder(&server);
  drop(server);

  let error = embedder
    .embed(EmbedKind::Document, &texts(&["one"]))
    .await
    .unwrap_err();

  assert!(matches!(error, EmbedError::Failed(_)), "{error}");
}

#[tokio::test]
async fn documents_are_scored_in_their_own_order() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/v1/rerank"))
    .and(body_json(json!({
      "query": "pie",
      "documents": ["apple tart", "apple pie"],
    })))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "model": "mock",
      "object": "list",
      "usage": {"prompt_tokens": 8, "total_tokens": 8},
      "results": [
        {"index": 1, "relevance_score": 0.9},
        {"index": 0, "relevance_score": 0.1},
      ],
    })))
    .expect(1)
    .mount(&server)
    .await;

  let scores = reranker(&server)
    .rerank("pie", &texts(&["apple tart", "apple pie"]))
    .await
    .unwrap();

  assert_eq!(scores, [0.1, 0.9]);
}

#[tokio::test]
async fn no_documents_means_no_request() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .respond_with(ResponseTemplate::new(500))
    .expect(0)
    .mount(&server)
    .await;

  let scores = reranker(&server).rerank("pie", &[]).await.unwrap();

  assert!(scores.is_empty());
}

#[tokio::test]
async fn a_reranker_that_refuses_is_a_failure_with_its_words() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .respond_with(
      ResponseTemplate::new(501).set_body_string("reranking is not enabled"),
    )
    .mount(&server)
    .await;

  let error = reranker(&server)
    .rerank("pie", &texts(&["apple pie"]))
    .await
    .unwrap_err();

  assert!(matches!(error, RerankError::Failed(_)), "{error}");
  assert!(
    error.to_string().contains("reranking is not enabled"),
    "{error}"
  );
}

#[tokio::test]
async fn scores_that_do_not_match_the_documents_are_a_bad_response() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "results": [{"index": 0, "relevance_score": 0.5}],
    })))
    .mount(&server)
    .await;

  let error = reranker(&server)
    .rerank("pie", &texts(&["apple pie", "apple tart"]))
    .await
    .unwrap_err();

  assert!(matches!(error, RerankError::BadResponse(_)), "{error}");
}
