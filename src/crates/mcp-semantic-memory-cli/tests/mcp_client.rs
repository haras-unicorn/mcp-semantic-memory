//! The server driven by a real MCP client over an in-memory pipe, on a real
//! in-memory SQLite store. Only the embedder is a stand-in.

#![cfg(feature = "sqlite")]
#![cfg(test)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mcp_semantic_memory::config::{
  ConsolidationConfig, DatabaseConfig, ReembedConfig, SearchConfig, WriteConfig,
};
use mcp_semantic_memory::prelude::*;
use mcp_semantic_memory::reembed::reembed;
use mcp_semantic_memory_sqlite::SqliteStore;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

type Client = RunningService<RoleClient, ClientConfig>;

const WORDS: [&str; 3] = ["apple", "pie", "tart"];

/// The words, and the constant.
const DIMENSION: usize = 4;

/// One axis per known word, and a small constant so no vector is all zeros.
struct WordEmbedder {
  fingerprint: Fingerprint,
}

#[async_trait]
impl Embedder for WordEmbedder {
  async fn embed(
    &self,
    _kind: EmbedKind,
    texts: &[String],
  ) -> Result<Vec<Vector>, EmbedError> {
    Ok(
      texts
        .iter()
        .map(|text| {
          let text = text.to_lowercase();
          let mut vector: Vector = WORDS
            .iter()
            .map(|word| if text.contains(word) { 1.0 } else { 0.0 })
            .collect();
          vector.push(0.1);
          vector
        })
        .collect(),
    )
  }

  fn fingerprint(&self) -> &Fingerprint {
    &self.fingerprint
  }
}

/// How many databases there have been, to give each its own name.
static DATABASES: AtomicUsize = AtomicUsize::new(0);

fn fingerprint(model: &str) -> Fingerprint {
  Fingerprint {
    model: model.to_owned(),
    dimension: DIMENSION,
  }
}

/// The URL of a new in-memory database that another store can open too. It
/// lives as long as one of them is open.
fn new_url() -> String {
  let name = DATABASES.fetch_add(1, Ordering::Relaxed);
  format!("sqlite:file:client_{name}?mode=memory&cache=shared")
}

async fn store(url: &str, model: &str) -> Box<dyn Store> {
  let database = DatabaseConfig {
    url: url.to_owned(),
    ..DatabaseConfig::default()
  };
  let store = SqliteStore::connect(&database, fingerprint(model))
    .await
    .unwrap();
  store.migrate().await.unwrap();
  Box::new(store)
}

/// A memory written straight to the store, with the vector the word embedder
/// would make for a text with just these words.
async fn remember(store: &dyn Store, text: &str, words: &[&str]) {
  let mut vector: Vector = WORDS
    .iter()
    .map(|word| if words.contains(word) { 1.0 } else { 0.0 })
    .collect();
  vector.push(0.1);
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
  let outcome = store.write(write).await.unwrap();
  assert!(matches!(outcome, WriteOutcome::Written(_)), "{outcome:?}");
}

async fn pause() {
  tokio::time::sleep(Duration::from_millis(30)).await;
}

/// A server over the store, with its startup checks run, and a client
/// connected to it.
async fn serve(
  store: Box<dyn Store>,
  model: &str,
  consolidation: ConsolidationConfig,
) -> Client {
  let embedder: Box<dyn Embedder> = Box::new(WordEmbedder {
    fingerprint: fingerprint(model),
  });
  let reranker: Box<dyn Reranker> = Box::new(NoReranker);
  let service = Service::new(
    embedder,
    reranker,
    store,
    WriteConfig::default(),
    SearchConfig::default(),
  )
  .with_consolidation(consolidation);
  let server = MemoryServer::new(service).checked().await.unwrap();

  let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
  tokio::spawn(async move {
    server.serve(server_transport).await?.waiting().await?;
    anyhow::Ok(())
  });
  ClientConfig::default()
    .serve(client_transport)
    .await
    .unwrap()
}

/// A client connected to a fresh server.
async fn connect(consolidation: ConsolidationConfig) -> Client {
  serve(store(&new_url(), "words").await, "words", consolidation).await
}

fn without_the_tool() -> ConsolidationConfig {
  ConsolidationConfig {
    has_tool: false,
    ..ConsolidationConfig::default()
  }
}

async fn call(
  client: &Client,
  tool: &'static str,
  arguments: Value,
) -> CallToolResult {
  let arguments = arguments.as_object().unwrap().clone();
  client
    .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
    .await
    .unwrap()
}

/// What a successful call answered.
fn answer(result: CallToolResult) -> Value {
  assert_ne!(result.is_error, Some(true), "{result:?}");
  result.structured_content.unwrap()
}

/// What a failed call said, as the model would read it.
fn failure(result: &CallToolResult) -> &str {
  assert_eq!(result.is_error, Some(true), "{result:?}");
  result
    .content
    .first()
    .and_then(|content| content.as_text())
    .map(|text| text.text.as_str())
    .unwrap()
}

async fn tool_names(client: &Client) -> Vec<String> {
  let listed = client.list_tools(None).await.unwrap();
  let mut names: Vec<String> = listed
    .tools
    .iter()
    .map(|tool| tool.name.to_string())
    .collect();
  names.sort();
  names
}

#[tokio::test]
async fn the_server_offers_its_three_tools_when_the_consolidation_is_off() {
  let client = connect(without_the_tool()).await;

  assert_eq!(
    tool_names(&client).await,
    ["memory_recall", "memory_search", "memory_store"]
  );

  client.cancel().await.unwrap();
}

#[tokio::test]
async fn the_server_offers_the_consolidation_tool_when_it_is_on() {
  let client = connect(ConsolidationConfig::default()).await;

  assert_eq!(
    tool_names(&client).await,
    [
      "memory_consolidate",
      "memory_recall",
      "memory_search",
      "memory_store"
    ]
  );

  client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_close_event_is_decided_on_the_second_call() {
  let client = connect(without_the_tool()).await;

  let first =
    answer(call(&client, "memory_store", json!({"event": "apple pie"})).await);
  assert_eq!(first["status"], "written");
  assert_eq!(first["memory_id"], 1);

  let asked = answer(
    call(&client, "memory_store", json!({"event": "apple pie again"})).await,
  );
  assert_eq!(asked["status"], "needs_decision");
  let candidate = &asked["candidates"][0]["memory"];
  assert_eq!(candidate["text"], "apple pie");

  let decided = answer(
    call(
      &client,
      "memory_store",
      json!({
        "event": "apple pie again",
        "decision": {
          "kind": "attach",
          "memory_id": candidate["memory_id"],
          "version": candidate["version"],
          "memory_text": "apple pie, twice",
        },
      }),
    )
    .await,
  );
  assert_eq!(decided["status"], "written");
  assert_eq!(decided["memory_id"], 1);

  let found =
    answer(call(&client, "memory_search", json!({"query": "pie"})).await);
  assert_eq!(found["results"][0]["memory"]["text"], "apple pie, twice");

  let recalled = answer(
    call(
      &client,
      "memory_recall",
      json!({"memory_id": 1, "with_events": true}),
    )
    .await,
  );
  assert_eq!(recalled["memory"]["event_count"], 2);
  assert_eq!(recalled["events"].as_array().unwrap().len(), 2);

  client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_missing_memory_is_an_error_the_model_can_read() {
  let client = connect(without_the_tool()).await;

  let result = call(&client, "memory_recall", json!({"memory_id": 7})).await;
  assert_eq!(failure(&result), "there is no memory 7");

  client.cancel().await.unwrap();
}

/// Two memories a moment apart. With a maximum age of a millisecond, the
/// newest event is too far ahead of the consolidation that has not happened.
async fn stale_store(url: &str) -> Box<dyn Store> {
  let store = store(url, "words").await;
  remember(&*store, "apple pie", &["apple", "pie"]).await;
  pause().await;
  remember(&*store, "tart", &["tart"]).await;
  store
}

fn picky() -> ConsolidationConfig {
  ConsolidationConfig {
    max_age: Duration::from_millis(1),
    ..ConsolidationConfig::default()
  }
}

#[tokio::test]
async fn a_stale_memory_is_fixed_by_the_consolidation_tool() {
  let client = serve(stale_store(&new_url()).await, "words", picky()).await;

  for (tool, arguments) in [
    ("memory_store", json!({"event": "apple tart"})),
    ("memory_search", json!({"query": "apple"})),
  ] {
    let refused = call(&client, tool, arguments).await;
    assert!(failure(&refused).contains("`memory_consolidate`"), "{tool}");
  }

  let done = answer(call(&client, "memory_consolidate", json!({})).await);
  assert_eq!(done["status"], "written");
  assert_eq!(done["memories"], 2);
  assert!(!done["interests"].as_array().unwrap().is_empty());

  let stored =
    answer(call(&client, "memory_store", json!({"event": "apple tart"})).await);
  assert!(stored["status"].is_string());

  client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_stale_memory_names_the_command_when_the_tool_is_off() {
  let config = ConsolidationConfig {
    has_tool: false,
    ..picky()
  };
  let client = serve(stale_store(&new_url()).await, "words", config).await;

  let refused =
    call(&client, "memory_store", json!({"event": "apple tart"})).await;
  assert!(failure(&refused).contains("`consolidate`"));
  assert!(!failure(&refused).contains("memory_consolidate"));

  client.cancel().await.unwrap();
}

#[tokio::test]
async fn two_consolidations_at_once_run_once() {
  let client = serve(stale_store(&new_url()).await, "words", picky()).await;

  let (first, second) = tokio::join!(
    call(&client, "memory_consolidate", json!({})),
    call(&client, "memory_consolidate", json!({})),
  );
  let mut statuses = [
    answer(first)["status"].as_str().unwrap().to_owned(),
    answer(second)["status"].as_str().unwrap().to_owned(),
  ];
  statuses.sort();
  assert_eq!(statuses, ["not_needed", "written"]);

  client.cancel().await.unwrap();
}

#[tokio::test]
async fn memories_to_reembed_keep_every_tool_out_even_the_consolidation() {
  let url = new_url();
  let old = store(&url, "words").await;
  remember(&*old, "apple pie", &["apple", "pie"]).await;

  // the same database, read by a server with another embedding model
  let client = serve(
    store(&url, "other").await,
    "other",
    ConsolidationConfig::default(),
  )
  .await;

  for (tool, arguments) in [
    ("memory_store", json!({"event": "apple pie"})),
    ("memory_search", json!({"query": "pie"})),
    ("memory_recall", json!({"memory_id": 1})),
    ("memory_consolidate", json!({})),
  ] {
    let result = call(&client, tool, arguments).await;
    assert!(failure(&result).contains("`reembed`"), "{tool}");
  }

  client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_reembed_lets_the_refusing_server_serve_again() {
  let url = new_url();
  let old = store(&url, "words").await;
  remember(&*old, "apple pie", &["apple", "pie"]).await;
  let client = serve(
    store(&url, "other").await,
    "other",
    ConsolidationConfig::default(),
  )
  .await;
  let refused = call(&client, "memory_search", json!({"query": "pie"})).await;
  assert!(failure(&refused).contains("`reembed`"));

  let embedder = WordEmbedder {
    fingerprint: fingerprint("other"),
  };
  let report = reembed(
    &*store(&url, "other").await,
    &embedder,
    &ReembedConfig::default(),
  )
  .await
  .unwrap();
  assert_eq!(report.embedded, 1);

  let found =
    answer(call(&client, "memory_search", json!({"query": "pie"})).await);
  assert_eq!(found["results"][0]["memory"]["text"], "apple pie");

  client.cancel().await.unwrap();
  drop(old);
}
