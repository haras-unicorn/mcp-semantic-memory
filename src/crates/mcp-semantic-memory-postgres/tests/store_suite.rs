//! The Postgres store against the checks every store has to pass, on a real
//! Postgres with `pgvector` in a container.

#![allow(
  clippy::unwrap_used,
  reason = "the helpers here fail by panicking, like the tests that call them, \
            but clippy only lets `#[test]` functions unwrap"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mcp_semantic_memory::config::DatabaseConfig;
use mcp_semantic_memory::store::Store;
use mcp_semantic_memory::types::{Fingerprint, MemoryId};
use mcp_semantic_memory_postgres::PostgresStore;
use mcp_semantic_memory_testing::containers::{self, Container};
use mcp_semantic_memory_testing::health::{self, Tamper};
use mcp_semantic_memory_testing::suite;
use sqlx::{AssertSqlSafe, PgPool};

mod common;

use common::{POSTGRES, url};

/// A server, and a new database on it for each check.
struct Server {
  container: Container,
  admin: PgPool,
  databases: AtomicUsize,
}

impl Server {
  async fn start() -> Self {
    let container = containers::start(&POSTGRES).await;
    let admin = PgPool::connect(&url(&container, "postgres")).await.unwrap();
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

  async fn fresh_store(&self) -> (PostgresStore, PgPool, String) {
    let url = self.fresh_database().await;
    let database = DatabaseConfig {
      url: url.clone(),
      ..DatabaseConfig::default()
    };
    let store = PostgresStore::connect(&database, suite::fingerprint())
      .await
      .unwrap();
    store.migrate().await.unwrap();
    let pool = PgPool::connect(&url).await.unwrap();
    (store, pool, url)
  }
}

#[tokio::test]
async fn the_postgres_store_passes_the_store_suite() {
  let server = Server::start().await;
  suite::run(async || {
    let store: Box<dyn Store> = Box::new(server.fresh_store().await.0);
    store
  })
  .await;
}

#[tokio::test]
async fn the_postgres_store_reports_its_health() {
  let server = Server::start().await;
  health::run(async || {
    let (store, pool, url) = server.fresh_store().await;
    let store: Box<dyn Store> = Box::new(store);
    let tamper: Box<dyn Tamper> = Box::new(PostgresTamper { pool, url });
    (store, tamper)
  })
  .await;
}

struct PostgresTamper {
  pool: PgPool,
  url: String,
}

#[async_trait]
impl Tamper for PostgresTamper {
  async fn reopen(&self, fingerprint: Fingerprint) -> Box<dyn Store> {
    let database = DatabaseConfig {
      url: self.url.clone(),
      ..DatabaseConfig::default()
    };
    let store = PostgresStore::connect(&database, fingerprint)
      .await
      .unwrap();
    Box::new(store)
  }

  async fn remake_vector(&self, id: MemoryId, fingerprint: &str) {
    sqlx::query(
      "UPDATE memory_vectors SET fingerprint = $2 WHERE memory_id = $1",
    )
    .bind(id.get())
    .bind(fingerprint)
    .execute(&self.pool)
    .await
    .unwrap();
  }

  async fn drop_vector(&self, id: MemoryId) {
    sqlx::query("DELETE FROM memory_vectors WHERE memory_id = $1")
      .bind(id.get())
      .execute(&self.pool)
      .await
      .unwrap();
  }

  async fn forget(&self, id: MemoryId) {
    sqlx::query("UPDATE memories SET forgotten_at = now() WHERE id = $1")
      .bind(id.get())
      .execute(&self.pool)
      .await
      .unwrap();
  }

  async fn add_interests(&self, ago: Duration, fingerprints: &[Option<&str>]) {
    let run_at: DateTime<Utc> =
      sqlx::query_scalar("SELECT now() - make_interval(secs => $1)")
        .bind(ago.as_secs_f64())
        .fetch_one(&self.pool)
        .await
        .unwrap();
    for fingerprint in fingerprints {
      let id: i64 = sqlx::query_scalar(
        "INSERT INTO interests (run_at, top_terms)
         VALUES ($1, 'cat')
         RETURNING id",
      )
      .bind(run_at)
      .fetch_one(&self.pool)
      .await
      .unwrap();
      if let Some(fingerprint) = fingerprint {
        sqlx::query(
          "INSERT INTO interest_vectors (interest_id, embedding, fingerprint)
           VALUES ($1, '[1.0, 0.0, 0.0]'::vector, $2)",
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
      "UPDATE memories SET created_at = now() - make_interval(secs => $1)",
    )
    .bind(ago.as_secs_f64())
    .execute(&self.pool)
    .await
    .unwrap();
  }

  async fn age_activity(&self, ago: Duration) {
    for statement in [
      "UPDATE memories SET
         created_at = created_at - make_interval(secs => $1),
         last_event_at = last_event_at - make_interval(secs => $1),
         updated_at = updated_at - make_interval(secs => $1)",
      "UPDATE events
       SET created_at = created_at - make_interval(secs => $1)",
    ] {
      sqlx::query(statement)
        .bind(ago.as_secs_f64())
        .execute(&self.pool)
        .await
        .unwrap();
    }
  }
}
