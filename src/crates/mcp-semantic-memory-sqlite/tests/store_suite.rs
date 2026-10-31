//! The SQLite store against the checks every store has to pass.

#![allow(
  clippy::unwrap_used,
  reason = "the helpers here fail by panicking, like the tests that call them, \
            but clippy only lets `#[test]` functions unwrap"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mcp_semantic_memory::config::DatabaseConfig;
use mcp_semantic_memory::store::Store;
use mcp_semantic_memory::types::{Fingerprint, MemoryId};
use mcp_semantic_memory_sqlite::SqliteStore;
use mcp_semantic_memory_testing::health::{self, Tamper};
use mcp_semantic_memory_testing::suite;
use sqlx::SqlitePool;

#[tokio::test]
async fn the_sqlite_store_passes_the_store_suite() {
  suite::run(async || {
    let fingerprint = Fingerprint {
      model: "test".to_owned(),
      dimension: suite::DIMENSION,
    };
    let database = DatabaseConfig {
      url: "sqlite::memory:".to_owned(),
      ..DatabaseConfig::default()
    };
    let store = SqliteStore::connect(&database, fingerprint).await.unwrap();
    store.migrate().await.unwrap();
    let store: Box<dyn Store> = Box::new(store);
    store
  })
  .await;
}

/// How many in-memory databases there have been, to give each its own name.
static DATABASES: AtomicUsize = AtomicUsize::new(0);

#[tokio::test]
async fn the_sqlite_store_reports_its_health() {
  health::run(async || {
    // A database in memory that a second connection can open too, as the
    // tamper has to, lives as long as one of them is open.
    let name = DATABASES.fetch_add(1, Ordering::Relaxed);
    let url = format!("sqlite:file:health_{name}?mode=memory&cache=shared");
    let database = DatabaseConfig {
      url: url.clone(),
      ..DatabaseConfig::default()
    };
    let store = SqliteStore::connect(&database, suite::fingerprint())
      .await
      .unwrap();
    store.migrate().await.unwrap();

    let pool = SqlitePool::connect(&url).await.unwrap();
    let store: Box<dyn Store> = Box::new(store);
    let tamper: Box<dyn Tamper> = Box::new(SqliteTamper { pool, url });
    (store, tamper)
  })
  .await;
}

struct SqliteTamper {
  pool: SqlitePool,
  url: String,
}

/// The modifier that moves a time back by `ago`.
fn back(ago: Duration) -> String {
  format!("-{} seconds", ago.as_secs())
}

#[async_trait]
impl Tamper for SqliteTamper {
  async fn remake_vector(&self, id: MemoryId, fingerprint: &str) {
    sqlx::query("DELETE FROM memory_vectors WHERE memory_id = ?1")
      .bind(id.get())
      .execute(&self.pool)
      .await
      .unwrap();
    sqlx::query(
      "INSERT INTO memory_vectors (memory_id, embedding, fingerprint)
       VALUES (?1, vec_f32('[0.0, 1.0, 0.0]'), ?2)",
    )
    .bind(id.get())
    .bind(fingerprint)
    .execute(&self.pool)
    .await
    .unwrap();
  }

  async fn drop_vector(&self, id: MemoryId) {
    sqlx::query("DELETE FROM memory_vectors WHERE memory_id = ?1")
      .bind(id.get())
      .execute(&self.pool)
      .await
      .unwrap();
  }

  async fn forget(&self, id: MemoryId) {
    sqlx::query(
      "UPDATE memories
       SET forgotten_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
       WHERE id = ?1",
    )
    .bind(id.get())
    .execute(&self.pool)
    .await
    .unwrap();
  }

  async fn add_interests(&self, ago: Duration, fingerprints: &[Option<&str>]) {
    let run_at: String =
      sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1)")
        .bind(back(ago))
        .fetch_one(&self.pool)
        .await
        .unwrap();
    for fingerprint in fingerprints {
      let id: i64 = sqlx::query_scalar(
        "INSERT INTO interests (run_at, top_terms)
         VALUES (?1, 'cat')
         RETURNING id",
      )
      .bind(&run_at)
      .fetch_one(&self.pool)
      .await
      .unwrap();
      if let Some(fingerprint) = fingerprint {
        sqlx::query(
          "INSERT INTO interest_vectors (interest_id, embedding, fingerprint)
           VALUES (?1, vec_f32('[1.0, 0.0, 0.0]'), ?2)",
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
       SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1)",
    )
    .bind(back(ago))
    .execute(&self.pool)
    .await
    .unwrap();
  }

  async fn reopen(&self, fingerprint: Fingerprint) -> Box<dyn Store> {
    let database = DatabaseConfig {
      url: self.url.clone(),
      ..DatabaseConfig::default()
    };
    let store = SqliteStore::connect(&database, fingerprint).await.unwrap();
    Box::new(store)
  }

  async fn age_activity(&self, ago: Duration) {
    sqlx::query(
      "UPDATE memories SET
         created_at = strftime('%Y-%m-%dT%H:%M:%fZ', created_at, ?1),
         last_event_at = strftime('%Y-%m-%dT%H:%M:%fZ', last_event_at, ?1),
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', updated_at, ?1)",
    )
    .bind(back(ago))
    .execute(&self.pool)
    .await
    .unwrap();
    sqlx::query(
      "UPDATE events
       SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', created_at, ?1)",
    )
    .bind(back(ago))
    .execute(&self.pool)
    .await
    .unwrap();
  }
}
