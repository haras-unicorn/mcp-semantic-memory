//! The Postgres store over TLS, on a Postgres whose certificate nobody signed.
//! What this shows is that the session is encrypted and that the certificate
//! is checked when asked for. A certificate that does verify, from a private
//! CA, is not tested.

use mcp_semantic_memory::config::DatabaseConfig;
use mcp_semantic_memory::store::Store;
use mcp_semantic_memory_postgres::PostgresStore;
use mcp_semantic_memory_testing::containers;
use mcp_semantic_memory_testing::suite;
use sqlx::PgPool;

mod common;

use common::{POSTGRES, url};

#[tokio::test]
async fn a_session_that_requires_tls_is_encrypted() {
  let container = containers::start(&POSTGRES).await;
  let admin = PgPool::connect(&url(&container, "postgres")).await.unwrap();
  sqlx::query("CREATE DATABASE tls")
    .execute(&admin)
    .await
    .unwrap();
  let url = format!("{}?sslmode=require", url(&container, "tls"));

  let pool = PgPool::connect(&url).await.unwrap();
  let encrypted: bool = sqlx::query_scalar(
    "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
  )
  .fetch_one(&pool)
  .await
  .unwrap();
  assert!(encrypted);

  let database = DatabaseConfig {
    url,
    ..DatabaseConfig::default()
  };
  let store = PostgresStore::connect(&database, suite::fingerprint())
    .await
    .unwrap();
  store.migrate().await.unwrap();
}

#[tokio::test]
async fn a_certificate_nobody_signed_is_not_trusted() {
  let container = containers::start(&POSTGRES).await;
  let url = format!("{}?sslmode=verify-full", url(&container, "postgres"));

  let error = PgPool::connect(&url).await.unwrap_err();

  assert!(matches!(error, sqlx::Error::Tls(_)), "{error}");
}
