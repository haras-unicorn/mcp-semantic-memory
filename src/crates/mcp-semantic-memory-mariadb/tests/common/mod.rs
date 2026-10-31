//! What the test files of this crate share.

use mcp_semantic_memory_testing::containers::{Container, Spec};

/// The MariaDB the tests start, built by Nix.
pub const MARIADB: Spec = Spec {
  archive: "MCP_SEMANTIC_MEMORY_TEST_MARIADB_ARCHIVE",
  image: "MCP_SEMANTIC_MEMORY_TEST_MARIADB_IMAGE",
  port: 3306,
  ready: "ready for connections",
  environment: &[],
};

/// The URL of a database on the container.
pub fn url(container: &Container, database: &str) -> String {
  format!(
    "mysql://root:root@{}:{}/{database}",
    container.host, container.port
  )
}
