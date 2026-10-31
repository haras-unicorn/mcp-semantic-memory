//! The MCP adapter: three tools over the service, and a fourth to consolidate
//! when the configuration asks for it, on stdio. It only turns tool arguments
//! into service calls and the answers into JSON.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::consolidation::{ForgottenMemory, InterestReport, Report, Status};
use crate::embedder::Embedder;
use crate::reranker::Reranker;
use crate::service::{Service, ServiceError};
use crate::store::{Problem, Store};
use crate::types::{
  Candidate, Decision, Event, Memory, MemoryId, NewEvent, Recall, SearchHit,
  StoreOutcome, Version,
};

/// The service with its facades chosen while the program runs.
pub type DynService =
  Service<Box<dyn Embedder>, Box<dyn Reranker>, Box<dyn Store>>;

/// The arguments of `memory_store`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StoreRequest {
  /// What happened or what was learned, as one self-contained piece of prose.
  pub event: String,
  /// Free form. Kept with the event as it is.
  pub metadata: Option<Value>,
  /// The answer to a `needs_decision` result. Leave it out on the first call
  /// and send it on the second, with the same `event` and `metadata`.
  pub decision: Option<DecisionRequest>,
}

/// What the event is, once the memory has shown the memories close to it.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecisionRequest {
  /// The event starts a memory of its own.
  New,
  /// The event belongs to one of the candidates.
  Attach {
    /// The `memory_id` of the candidate.
    memory_id: i64,
    /// The `version` the candidate had in the result.
    version: i64,
    /// What the memory says from now on, in full: its old text rewritten to
    /// include the event, and to correct whatever the event contradicts.
    memory_text: String,
  },
  /// The event only repeats one of the candidates, whose text stays.
  Repeat {
    /// The `memory_id` of the candidate.
    memory_id: i64,
    /// The `version` the candidate had in the result.
    version: i64,
  },
}

impl From<DecisionRequest> for Decision {
  fn from(request: DecisionRequest) -> Self {
    match request {
      DecisionRequest::New => Self::New,
      DecisionRequest::Attach {
        memory_id,
        version,
        memory_text,
      } => Self::Attach {
        memory_id: MemoryId::new(memory_id),
        version: Version::new(version),
        memory_text,
      },
      DecisionRequest::Repeat { memory_id, version } => Self::Repeat {
        memory_id: MemoryId::new(memory_id),
        version: Version::new(version),
      },
    }
  }
}

/// The arguments of `memory_search`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
  /// What to look for, in words or as a question.
  pub query: String,
  /// The most results to return. The server has a default.
  pub limit: Option<usize>,
}

/// The arguments of `memory_recall`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallRequest {
  /// The `memory_id` of the memory.
  pub memory_id: i64,
  /// Also return the events told to the memory, newest first. Off by default.
  pub with_events: Option<bool>,
}

/// A memory as the model sees it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MemoryView {
  /// The id of the memory.
  pub memory_id: i64,
  /// What the memory says.
  pub text: String,
  /// Changes with every write to the memory. Send it back in a decision.
  pub version: i64,
  /// How many events have been told to the memory.
  pub event_count: i64,
  /// When the latest event was told, as an RFC 3339 time.
  pub last_event_at: String,
}

impl From<Memory> for MemoryView {
  fn from(memory: Memory) -> Self {
    Self {
      memory_id: memory.id.get(),
      text: memory.content,
      version: memory.version.get(),
      event_count: memory.event_count,
      last_event_at: memory.last_event_at.to_rfc3339(),
    }
  }
}

/// An event as the model sees it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EventView {
  /// The id of the event.
  pub event_id: i64,
  /// What was told.
  pub content: String,
  /// What came with it.
  pub metadata: Value,
  /// What the memory said after this event.
  pub memory_text: String,
  /// When it was told, as an RFC 3339 time.
  pub told_at: String,
}

impl From<Event> for EventView {
  fn from(event: Event) -> Self {
    Self {
      event_id: event.id.get(),
      content: event.content,
      metadata: event.metadata,
      memory_text: event.memory_text,
      told_at: event.created_at.to_rfc3339(),
    }
  }
}

/// Whether `memory_store` wrote the event.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StoreStatus {
  /// The event is stored, and `memory_id` says in which memory.
  Written,
  /// Nothing is stored yet. `candidates` are the memories it may belong to.
  NeedsDecision,
}

/// A memory the event may belong to.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CandidateView {
  /// The memory.
  pub memory: MemoryView,
  /// How far the memory is from the event. Lower is closer.
  pub distance: f32,
}

impl From<Candidate> for CandidateView {
  fn from(candidate: Candidate) -> Self {
    Self {
      memory: candidate.memory.into(),
      distance: candidate.distance,
    }
  }
}

/// The answer of `memory_store`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StoreOutput {
  /// Whether the event is stored.
  pub status: StoreStatus,
  /// The memory the event is in, when it is stored.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub memory_id: Option<i64>,
  /// What to do next, when nothing is stored yet.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
  /// The memories the event may belong to, closest first.
  pub candidates: Vec<CandidateView>,
}

impl From<StoreOutcome> for StoreOutput {
  fn from(outcome: StoreOutcome) -> Self {
    match outcome {
      StoreOutcome::Written { memory_id } => Self {
        status: StoreStatus::Written,
        memory_id: Some(memory_id.get()),
        message: None,
        candidates: Vec::new(),
      },
      StoreOutcome::NeedsDecision { candidates } => Self {
        status: StoreStatus::NeedsDecision,
        memory_id: None,
        message: Some(
          "Nothing is stored yet. Call memory_store again with the same \
           event and metadata and a decision: new if the event is a memory \
           of its own, attach if it belongs to a candidate (with the \
           candidate's memory_id and version, and the memory's rewritten \
           text), or repeat if it only repeats a candidate."
            .to_owned(),
        ),
        candidates: candidates.into_iter().map(CandidateView::from).collect(),
      },
    }
  }
}

/// A memory found by `memory_search`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct HitView {
  /// The memory.
  pub memory: MemoryView,
  /// Higher is a better match. Only means something next to the other
  /// scores of the same search.
  pub score: f32,
}

impl From<SearchHit> for HitView {
  fn from(hit: SearchHit) -> Self {
    Self {
      memory: hit.memory.into(),
      score: hit.score,
    }
  }
}

/// The answer of `memory_search`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchOutput {
  /// The best match first.
  pub results: Vec<HitView>,
}

/// The answer of `memory_recall`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecallOutput {
  /// The memory.
  pub memory: MemoryView,
  /// The events told to the memory, newest first. Only there when asked for.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub events: Option<Vec<EventView>>,
}

impl From<Recall> for RecallOutput {
  fn from(recall: Recall) -> Self {
    Self {
      memory: recall.memory.into(),
      events: recall
        .events
        .map(|events| events.into_iter().map(EventView::from).collect()),
    }
  }
}

/// One interest of a consolidation, as the model sees it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InterestView {
  /// The words that name the interest.
  pub top_terms: Vec<String>,
  /// How many memories belong to it.
  pub memories: usize,
  /// What the memory closest to its centre says.
  pub nearest: Option<String>,
}

impl From<InterestReport> for InterestView {
  fn from(interest: InterestReport) -> Self {
    Self {
      top_terms: interest.top_terms,
      memories: interest.memories,
      nearest: interest.nearest,
    }
  }
}

/// A memory a consolidation forgot, as the model sees it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ForgottenView {
  /// The id of the memory.
  pub memory_id: i64,
  /// What it said.
  pub text: String,
}

impl From<ForgottenMemory> for ForgottenView {
  fn from(memory: ForgottenMemory) -> Self {
    Self {
      memory_id: memory.memory_id.get(),
      text: memory.text,
    }
  }
}

/// The answer of `memory_consolidate`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ConsolidateOutput {
  /// `written` when the upkeep ran, `not_needed` when another run had just
  /// done it, and `nothing_to_consolidate` when there are no memories.
  pub status: &'static str,
  /// How many active memories there were.
  pub memories: usize,
  /// What the memory is interested in now.
  pub interests: Vec<InterestView>,
  /// The memories that faded and are forgotten. They come back when an
  /// event is attached to them again.
  pub forgotten: Vec<ForgottenView>,
}

impl From<Report> for ConsolidateOutput {
  fn from(report: Report) -> Self {
    Self {
      status: match report.status {
        Status::Written => "written",
        Status::NotNeeded => "not_needed",
        Status::NothingToConsolidate => "nothing_to_consolidate",
        Status::DryRun => "dry_run",
      },
      memories: report.memories,
      interests: report
        .interests
        .into_iter()
        .map(InterestView::from)
        .collect(),
      forgotten: report
        .forgotten
        .into_iter()
        .map(ForgottenView::from)
        .collect(),
    }
  }
}

/// The MCP server. It holds no state of its own, only whether the startup
/// checks found something to fix, which it looks at again until that is
/// fixed.
#[derive(Clone)]
pub struct MemoryServer {
  service: Arc<DynService>,
  tool_router: ToolRouter<Self>,
  refusing: Arc<AtomicBool>,
}

impl MemoryServer {
  /// Serves the tools over this service. The consolidation tool is only
  /// there when the service's configuration has it on.
  pub fn new(service: DynService) -> Self {
    let mut tool_router = Self::tool_router();
    if !service.consolidation().has_tool {
      tool_router.remove_route(CONSOLIDATE_TOOL);
    }
    Self {
      service: Arc::new(service),
      tool_router,
      refusing: Arc::new(AtomicBool::new(false)),
    }
  }

  /// Runs the startup checks. When they find a problem the server still
  /// answers the handshake, but every tool call fails with the problem, so
  /// the model and the human both see what has to be fixed. The consolidation
  /// tool is let through when a consolidation fixes the problem. The checks
  /// run again on each call until they pass, so fixing the problem, with the
  /// tool or from outside, needs no restart.
  pub async fn checked(self) -> Result<Self, ServiceError> {
    if let Some(problem) = self.service.problem().await? {
      tracing::error!(
        problem = %problem.message,
        "serving, but every tool call will refuse"
      );
      self.refusing.store(true, Ordering::Relaxed);
    }
    Ok(self)
  }

  /// What is wrong, if the server is refusing. It forgets the problem when
  /// it is gone.
  async fn refusal(&self) -> Result<Option<Problem>, ServiceError> {
    if !self.refusing.load(Ordering::Relaxed) {
      return Ok(None);
    }
    let problem = self.service.problem().await?;
    if problem.is_none() {
      self.refusing.store(false, Ordering::Relaxed);
    }
    Ok(problem)
  }

  async fn ready(&self) -> Result<(), ServiceError> {
    match self.refusal().await? {
      Some(problem) => Err(ServiceError::NotReady(problem.message)),
      None => Ok(()),
    }
  }

  async fn ready_to_consolidate(&self) -> Result<(), ServiceError> {
    match self.refusal().await? {
      Some(problem) if !problem.consolidation_fixes_it => {
        Err(ServiceError::NotReady(problem.message))
      }
      _ => Ok(()),
    }
  }

  /// What `memory_store` does.
  pub async fn store(
    &self,
    request: StoreRequest,
  ) -> Result<StoreOutput, ServiceError> {
    self.ready().await?;
    let event = NewEvent {
      content: request.event,
      metadata: request.metadata.unwrap_or_default(),
    };
    let decision = request.decision.map(Decision::from);
    let outcome = self.service.store(event, decision).await?;
    Ok(outcome.into())
  }

  /// What `memory_search` does.
  pub async fn search(
    &self,
    request: SearchRequest,
  ) -> Result<SearchOutput, ServiceError> {
    self.ready().await?;
    let hits = self.service.search(&request.query, request.limit).await?;
    Ok(SearchOutput {
      results: hits.into_iter().map(HitView::from).collect(),
    })
  }

  /// What `memory_recall` does. There is nothing when no such memory is
  /// there.
  pub async fn recall(
    &self,
    request: RecallRequest,
  ) -> Result<Option<RecallOutput>, ServiceError> {
    self.ready().await?;
    let recall = self
      .service
      .recall(
        MemoryId::new(request.memory_id),
        request.with_events.unwrap_or(false),
      )
      .await?;
    Ok(recall.map(RecallOutput::from))
  }

  /// What `memory_consolidate` does: the upkeep, unless another run just did
  /// it.
  pub async fn consolidate(&self) -> Result<ConsolidateOutput, ServiceError> {
    self.ready_to_consolidate().await?;
    Ok(self.service.consolidate().await?.into())
  }
}

// NOTE: the router is built from the tools above and the name has to be
// repeated to take the tool out again
const CONSOLIDATE_TOOL: &str = "memory_consolidate";

#[tool_router]
impl MemoryServer {
  #[tool(
    description = "Tell the memory something worth remembering later, as one \
      self-contained event in plain prose. Send just the event first. If no \
      stored memory is close to it, it is written. Otherwise nothing is \
      stored yet and the result lists the close memories: call again with \
      the same event and a `decision`, as the result's message says. To \
      correct a memory, attach an event that says so, with the memory's \
      corrected text. Text over the length limit is refused: condense it \
      and call again."
  )]
  async fn memory_store(
    &self,
    Parameters(request): Parameters<StoreRequest>,
  ) -> Result<Json<StoreOutput>, String> {
    self.store(request).await.map(Json).map_err(describe)
  }

  #[tool(
    description = "Find memories by meaning and by words, best match first. \
      Searching changes nothing."
  )]
  async fn memory_search(
    &self,
    Parameters(request): Parameters<SearchRequest>,
  ) -> Result<Json<SearchOutput>, String> {
    self.search(request).await.map(Json).map_err(describe)
  }

  #[tool(
    description = "Get one memory by its memory_id. With with_events it also \
      returns the events told to the memory, newest first, so you can see \
      how it came to say what it says."
  )]
  async fn memory_recall(
    &self,
    Parameters(request): Parameters<RecallRequest>,
  ) -> Result<Json<RecallOutput>, String> {
    let memory_id = request.memory_id;
    match self.recall(request).await.map_err(describe)? {
      Some(output) => Ok(Json(output)),
      None => Err(format!("there is no memory {memory_id}")),
    }
  }

  #[tool(
    description = "Run the memory's upkeep: find what it is interested in \
      now and forget memories that faded. Only call it after another memory \
      tool failed because the last consolidation is too old. It takes no \
      arguments and may take a while."
  )]
  async fn memory_consolidate(
    &self,
  ) -> Result<Json<ConsolidateOutput>, String> {
    self.consolidate().await.map(Json).map_err(describe)
  }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MemoryServer {
  fn get_info(&self) -> ServerConfig {
    ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
      .with_server_info(Implementation::new(
        "mcp-semantic-memory",
        env!("CARGO_PKG_VERSION"),
      ))
      .with_instructions(
        "A long-term memory. Tell it things worth keeping with memory_store, \
         find them again with memory_search, and read one in full with \
         memory_recall.",
      )
  }
}

/// A failed call is a tool error with its cause as the text, not a protocol
/// error, so the model can read it and react.
fn describe(error: ServiceError) -> String {
  tracing::warn!(%error, "a tool call failed");
  error.to_string()
}

#[cfg(test)]
mod tests {
  use chrono::TimeDelta;

  use super::*;
  use crate::config::{ConsolidationConfig, SearchConfig, WriteConfig};
  use crate::fakes::{FakeEmbedder, FakeReranker, FakeStore};

  fn server() -> MemoryServer {
    server_over(FakeStore::new(), ConsolidationConfig::default())
  }

  fn server_over(
    store: FakeStore,
    consolidation: ConsolidationConfig,
  ) -> MemoryServer {
    let embedder: Box<dyn Embedder> = Box::new(
      FakeEmbedder::new(3)
        .with_word("apple", vec![1.0, 0.0, 0.0])
        .with_word("pie", vec![0.0, 1.0, 0.0])
        .with_word("tart", vec![0.0, 0.0, 1.0]),
    );
    let reranker: Box<dyn Reranker> = Box::new(FakeReranker);
    let store: Box<dyn Store> = Box::new(store);
    MemoryServer::new(
      Service::new(
        embedder,
        reranker,
        store,
        WriteConfig::default(),
        SearchConfig::default(),
      )
      .with_consolidation(consolidation),
    )
  }

  /// A server whose newest event is a month ahead of the oldest memory, with
  /// no consolidation in between, and that has run its startup checks.
  async fn stale_server(has_tool: bool) -> MemoryServer {
    let store = FakeStore::new();
    store.add("apple pie", vec![1.0, 0.0, 0.0]).await;
    store.age_activity(TimeDelta::days(30)).await;
    store.add("tart", vec![0.0, 0.0, 1.0]).await;
    let consolidation = ConsolidationConfig {
      has_tool,
      ..ConsolidationConfig::default()
    };
    server_over(store, consolidation).checked().await.unwrap()
  }

  fn store_request(
    event: &str,
    decision: Option<DecisionRequest>,
  ) -> StoreRequest {
    StoreRequest {
      event: event.to_owned(),
      metadata: None,
      decision,
    }
  }

  #[tokio::test]
  async fn a_first_event_is_written() {
    let output = server()
      .store(store_request("apple pie", None))
      .await
      .unwrap();
    assert_eq!(output.status, StoreStatus::Written);
    assert_eq!(output.memory_id, Some(1));
    assert!(output.message.is_none());
    assert!(output.candidates.is_empty());
  }

  #[tokio::test]
  async fn a_close_event_needs_a_decision_and_attaching_answers_it() {
    let server = server();
    server
      .store(store_request("apple pie", None))
      .await
      .unwrap();

    let asked = server
      .store(store_request("apple pie again", None))
      .await
      .unwrap();
    assert_eq!(asked.status, StoreStatus::NeedsDecision);
    assert!(asked.message.is_some());
    let candidate = asked.candidates.first().unwrap();
    assert_eq!(candidate.memory.text, "apple pie");

    let decision = DecisionRequest::Attach {
      memory_id: candidate.memory.memory_id,
      version: candidate.memory.version,
      memory_text: "apple pie, twice".to_owned(),
    };
    let answered = server
      .store(store_request("apple pie again", Some(decision)))
      .await
      .unwrap();
    assert_eq!(answered.status, StoreStatus::Written);
    assert_eq!(answered.memory_id, Some(1));

    let recalled = server
      .recall(RecallRequest {
        memory_id: 1,
        with_events: Some(true),
      })
      .await
      .unwrap()
      .unwrap();
    assert_eq!(recalled.memory.text, "apple pie, twice");
    assert_eq!(recalled.memory.event_count, 2);
    let events = recalled.events.unwrap();
    assert_eq!(events.first().unwrap().content, "apple pie again");
    assert_eq!(events.len(), 2);
  }

  #[tokio::test]
  async fn the_metadata_is_kept_with_the_event() {
    let server = server();
    server
      .store(StoreRequest {
        event: "apple pie".to_owned(),
        metadata: Some(serde_json::json!({"session": "monday"})),
        decision: None,
      })
      .await
      .unwrap();

    let recalled = server
      .recall(RecallRequest {
        memory_id: 1,
        with_events: Some(true),
      })
      .await
      .unwrap()
      .unwrap();
    let events = recalled.events.unwrap();
    assert_eq!(
      events.first().unwrap().metadata,
      serde_json::json!({"session": "monday"})
    );
  }

  #[tokio::test]
  async fn recalling_without_events_leaves_them_out() {
    let server = server();
    server
      .store(store_request("apple pie", None))
      .await
      .unwrap();
    let recalled = server
      .recall(RecallRequest {
        memory_id: 1,
        with_events: None,
      })
      .await
      .unwrap()
      .unwrap();
    assert!(recalled.events.is_none());
  }

  #[tokio::test]
  async fn recalling_a_memory_that_is_not_there_finds_nothing() {
    let recalled = server()
      .recall(RecallRequest {
        memory_id: 7,
        with_events: None,
      })
      .await
      .unwrap();
    assert!(recalled.is_none());
  }

  #[tokio::test]
  async fn a_search_finds_the_memory_by_its_words() {
    let server = server();
    server
      .store(store_request("apple pie", None))
      .await
      .unwrap();
    server.store(store_request("tart", None)).await.unwrap();

    let output = server
      .search(SearchRequest {
        query: "pie".to_owned(),
        limit: Some(1),
      })
      .await
      .unwrap();
    assert_eq!(output.results.len(), 1);
    assert_eq!(output.results.first().unwrap().memory.text, "apple pie");
  }

  #[tokio::test]
  async fn a_server_that_is_not_ready_says_why_on_every_call() {
    let store = FakeStore::new();
    store.set_to_reembed(2).await;
    let server = server_over(store, ConsolidationConfig::default())
      .checked()
      .await
      .unwrap();

    let stored = server
      .store(store_request("apple pie", None))
      .await
      .unwrap_err();
    assert!(matches!(stored, ServiceError::NotReady(_)), "{stored}");
    assert!(stored.to_string().contains("Run `reembed`."), "{stored}");

    let searched = server
      .search(SearchRequest {
        query: "pie".to_owned(),
        limit: None,
      })
      .await
      .unwrap_err();
    assert!(matches!(searched, ServiceError::NotReady(_)), "{searched}");

    let recalled = server
      .recall(RecallRequest {
        memory_id: 1,
        with_events: None,
      })
      .await
      .unwrap_err();
    assert!(matches!(recalled, ServiceError::NotReady(_)), "{recalled}");
  }

  #[tokio::test]
  async fn a_stale_memory_refuses_all_but_the_consolidation_which_fixes_it() {
    let server = stale_server(true).await;

    let refused = server
      .store(store_request("apple tart", None))
      .await
      .unwrap_err();
    assert!(
      refused.to_string().contains("`memory_consolidate`"),
      "{refused}"
    );

    let output = server.consolidate().await.unwrap();
    assert_eq!(output.status, "written");
    assert_eq!(output.memories, 2);
    assert!(!output.interests.is_empty());

    let stored = server.store(store_request("apple tart", None)).await;
    assert!(stored.is_ok(), "{stored:?}");
  }

  #[tokio::test]
  async fn without_the_tool_the_refusal_names_the_command() {
    let server = stale_server(false).await;
    let refused = server
      .store(store_request("apple tart", None))
      .await
      .unwrap_err()
      .to_string();
    assert!(refused.contains("`consolidate`"), "{refused}");
    assert!(!refused.contains("memory_consolidate"), "{refused}");
  }

  #[tokio::test]
  async fn memories_to_reembed_keep_the_consolidation_out_too() {
    let store = FakeStore::new();
    store.add("apple pie", vec![1.0, 0.0, 0.0]).await;
    store.set_to_reembed(1).await;
    let server = server_over(store, ConsolidationConfig::default())
      .checked()
      .await
      .unwrap();
    let refused = server.consolidate().await.unwrap_err().to_string();
    assert!(refused.contains("`reembed`"), "{refused}");
  }

  #[tokio::test]
  async fn the_consolidation_runs_once_when_two_ask_at_once() {
    let server = stale_server(true).await;
    let (first, second) =
      tokio::join!(server.consolidate(), server.consolidate());
    let mut statuses = [first.unwrap().status, second.unwrap().status];
    statuses.sort_unstable();
    assert_eq!(statuses, ["not_needed", "written"]);
  }

  #[tokio::test]
  async fn an_empty_event_is_an_error() {
    let error = server().store(store_request("  ", None)).await.unwrap_err();
    assert!(matches!(error, ServiceError::Empty { .. }), "{error}");
  }

  #[test]
  fn a_decision_is_read_from_the_arguments_of_a_tool_call() {
    let request: StoreRequest = serde_json::from_value(serde_json::json!({
      "event": "apple pie",
      "decision": {
        "kind": "attach",
        "memory_id": 3,
        "version": 2,
        "memory_text": "apple pie, baked"
      }
    }))
    .unwrap();
    assert_eq!(
      Decision::from(request.decision.unwrap()),
      Decision::Attach {
        memory_id: MemoryId::new(3),
        version: Version::new(2),
        memory_text: "apple pie, baked".to_owned(),
      }
    );
  }

  #[test]
  fn a_repeat_and_a_new_decision_are_read_too() {
    let repeat: DecisionRequest = serde_json::from_value(
      serde_json::json!({"kind": "repeat", "memory_id": 3, "version": 2}),
    )
    .unwrap();
    assert_eq!(
      Decision::from(repeat),
      Decision::Repeat {
        memory_id: MemoryId::new(3),
        version: Version::new(2),
      }
    );
    let new: DecisionRequest =
      serde_json::from_value(serde_json::json!({"kind": "new"})).unwrap();
    assert_eq!(Decision::from(new), Decision::New);
  }
}
