//! The MariaDB store over TLS, on a MariaDB whose certificate nobody signed.
//! What this shows is that the session is encrypted and that the certificate
//! is checked when asked for. A certificate that does verify, from a private
//! CA, is not tested.

use mcp_semantic_memory::config::DatabaseConfig;
use mcp_semantic_memory::store::Store;
use mcp_semantic_memory_mariadb::MariaDbStore;
use mcp_semantic_memory_testing::containers;
use mcp_semantic_memory_testing::suite;
use sqlx::MySqlPool;

mod common;

use common::{MARIADB, url};

#[tokio::test]
async fn a_session_that_requires_tls_is_encrypted() {
  let container = containers::start(&MARIADB).await;
  let admin = MySqlPool::connect(&url(&container, "mysql")).await.unwrap();
  sqlx::query("CREATE DATABASE tls")
    .execute(&admin)
    .await
    .unwrap();
  let url = format!("{}?ssl-mode=required", url(&container, "tls"));

  let pool = MySqlPool::connect(&url).await.unwrap();
  let cipher: String = sqlx::query_scalar(
    "SELECT VARIABLE_VALUE FROM information_schema.SESSION_STATUS
     WHERE VARIABLE_NAME = 'Ssl_cipher'",
  )
  .fetch_one(&pool)
  .await
  .unwrap();
  assert!(!cipher.is_empty());

  let database = DatabaseConfig {
    url,
    ..DatabaseConfig::default()
  };
  let store = MariaDbStore::connect(&database, suite::fingerprint())
    .await
    .unwrap();
  store.migrate().await.unwrap();
}

#[tokio::test]
async fn a_certificate_nobody_signed_is_not_trusted() {
  let container = containers::start(&MARIADB).await;
  let url = format!("{}?ssl-mode=verify_identity", url(&container, "mysql"));

  let error = MySqlPool::connect(&url).await.unwrap_err();

  assert!(matches!(error, sqlx::Error::Tls(_)), "{error}");
}
