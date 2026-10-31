//! What most users of the library need: the configuration, the service, the
//! three facades and the types that go through them.

pub use crate::config::{Config, Dialect};
pub use crate::embedder::{EmbedError, EmbedKind, Embedder};
pub use crate::jina::JinaReranker;
pub use crate::openai::OpenAiEmbedder;
pub use crate::reranker::{NoReranker, RerankError, Reranker};
pub use crate::server::{DynService, MemoryServer};
pub use crate::service::{Service, ServiceError};
pub use crate::store::{
  Forgotten, Health, MemoryText, Store, StoreError, Write, WriteOutcome,
};
pub use crate::types::{
  Candidate, Decision, Event, EventId, Fingerprint, Memory, MemoryId, NewEvent,
  Recall, SearchHit, StoreOutcome, Vector, Version,
};
