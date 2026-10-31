//! A stateless MCP server for long-term semantic memory.

pub mod cluster;
pub mod config;
pub mod consolidation;
pub mod embedder;
#[cfg(test)]
mod fakes;
mod http;
pub mod jina;
pub mod openai;
pub mod prelude;
pub mod ranking;
pub mod reembed;
pub mod reranker;
pub mod server;
pub mod service;
pub mod store;
pub mod types;
pub mod vector;
