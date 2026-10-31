//! What the test files of this crate share.

use mcp_semantic_memory_testing::containers::{Container, Spec};

/// The Postgres the tests start, built by Nix.
pub const POSTGRES: Spec = Spec {
  archive: "MCP_SEMANTIC_MEMORY_TEST_POSTGRES_ARCHIVE",
  image: "MCP_SEMANTIC_MEMORY_TEST_POSTGRES_IMAGE",
  port: 5432,
  ready: "database system is ready to accept connections",
  environment: &[],
};

/// The URL of a database on the container.
pub fn url(container: &Container, database: &str) -> String {
  format!(
    "postgres://postgres@{}:{}/{database}",
    container.host, container.port
  )
}
