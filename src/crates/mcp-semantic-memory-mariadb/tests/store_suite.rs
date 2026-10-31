//! The MariaDB store against the checks every store has to pass, on a real
//! MariaDB in a container.

#![allow(
  clippy::unwrap_used,
  reason = "the helpers here fail by panicking, like the tests that call them, \
            but clippy only lets `#[test]` functions unwrap"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::NaiveDateTime;
use mcp_semantic_memory::config::DatabaseConfig;
use mcp_semantic_memory::store::Store;
use mcp_semantic_memory::types::{Fingerprint, MemoryId};
use mcp_semantic_memory_mariadb::MariaDbStore;
use mcp_semantic_memory_testing::containers::{self, Container};
use mcp_semantic_memory_testing::health::{self, Tamper};
use mcp_semantic_memory_testing::suite;
use sqlx::{AssertSqlSafe, MySqlPool};

mod common;

use common::{MARIADB, url};

/// A server, and a new database on it for each check.
struct Server {
  container: Container,
  admin: MySqlPool,
  databases: AtomicUsize,
}

impl Server {
  async fn start() -> Self {
    let container = containers::start(&MARIADB).await;
    let admin = MySqlPool::connect(&url(&container, "mysql")).await.unwrap();
    Self {
      container,
      admin,
      databases: AtomicUsize::new(0),
    }
  }

  async fn fresh_database(&self) -> String {
    let name =
      format!("check_{}", self.databases.fetch_add(1, Ordering::Relaxed));
    // The name is made above.
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
      .execute(&self.admin)
      .await
      .unwrap();
    url(&self.container, &name)
  }

  async fn fresh_store(&self) -> (MariaDbStore, MySqlPool, String) {
    let url = self.fresh_database().await;
    let database = DatabaseConfig {
      url: url.clone(),
      ..DatabaseConfig::default()
    };
    let store = MariaDbStore::connect(&database, suite::fingerprint())
      .await
      .unwrap();
    store.migrate().await.unwrap();
    let pool = MySqlPool::connect(&url).await.unwrap();
    (store, pool, url)
  }
}

#[tokio::test]
async fn the_mariadb_store_passes_the_store_suite() {
  let server = Server::start().await;
  suite::run(async || {
    let store: Box<dyn Store> = Box::new(server.fresh_store().await.0);
    store
  })
  .await;
}

#[tokio::test]
async fn the_mariadb_store_reports_its_health() {
  let server = Server::start().await;
  health::run(async || {
    let (store, pool, url) = server.fresh_store().await;
    let store: Box<dyn Store> = Box::new(store);
    let tamper: Box<dyn Tamper> = Box::new(MariaDbTamper { pool, url });
    (store, tamper)
  })
  .await;
}

/// The seconds of a duration, as the interval of a statement.
fn seconds(duration: Duration) -> i64 {
  i64::try_from(duration.as_secs()).unwrap()
}

struct MariaDbTamper {
  pool: MySqlPool,
  url: String,
}

#[async_trait]
impl Tamper for MariaDbTamper {
  async fn reopen(&self, fingerprint: Fingerprint) -> Box<dyn Store> {
    let database = DatabaseConfig {
      url: self.url.clone(),
      ..DatabaseConfig::default()
    };
    let store = MariaDbStore::connect(&database, fingerprint).await.unwrap();
    Box::new(store)
  }

  async fn remake_vector(&self, id: MemoryId, fingerprint: &str) {
    sqlx::query(
      "UPDATE memory_vectors SET fingerprint = ? WHERE memory_id = ?",
    )
    .bind(fingerprint)
    .bind(id.get())
    .execute(&self.pool)
    .await
    .unwrap();
  }

  async fn drop_vector(&self, id: MemoryId) {
    sqlx::query("DELETE FROM memory_vectors WHERE memory_id = ?")
      .bind(id.get())
      .execute(&self.pool)
      .await
      .unwrap();
  }

  async fn forget(&self, id: MemoryId) {
    sqlx::query(
      "UPDATE memories SET forgotten_at = CURRENT_TIMESTAMP(6) WHERE id = ?",
    )
    .bind(id.get())
    .execute(&self.pool)
    .await
    .unwrap();
  }

  async fn add_interests(&self, ago: Duration, fingerprints: &[Option<&str>]) {
    let run_at: NaiveDateTime = sqlx::query_scalar(
      "SELECT DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? SECOND)",
    )
    .bind(seconds(ago))
    .fetch_one(&self.pool)
    .await
    .unwrap();
    for fingerprint in fingerprints {
      let id = sqlx::query(
        "INSERT INTO interests (run_at, top_terms) VALUES (?, 'cat')",
      )
      .bind(run_at)
      .execute(&self.pool)
      .await
      .unwrap()
      .last_insert_id();
      if let Some(fingerprint) = fingerprint {
        sqlx::query(
          "INSERT INTO interest_vectors (interest_id, embedding, fingerprint)
           VALUES (?, VEC_FromText('[1.0, 0.0, 0.0]'), ?)",
        )
        .bind(id)
        .bind(fingerprint)
        .execute(&self.pool)
        .await
        .unwrap();
      }
    }
  }

  async fn age_memories(&self, ago: Duration) {
    sqlx::query(
      "UPDATE memories
       SET created_at = DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? SECOND)",
    )
    .bind(seconds(ago))
    .execute(&self.pool)
    .await
    .unwrap();
  }

  async fn age_activity(&self, ago: Duration) {
    for statement in [
      "UPDATE memories SET
         created_at = DATE_SUB(created_at, INTERVAL ? SECOND),
         last_event_at = DATE_SUB(last_event_at, INTERVAL ? SECOND),
         updated_at = DATE_SUB(updated_at, INTERVAL ? SECOND)",
      "UPDATE events
       SET created_at = DATE_SUB(created_at, INTERVAL ? SECOND)",
    ] {
      let mut query = sqlx::query(statement);
      for _ in statement.matches('?') {
        query = query.bind(seconds(ago));
      }
      query.execute(&self.pool).await.unwrap();
    }
  }
}
