//! What each subcommand does.

use std::io::Write as _;

use anyhow::{Result, bail};
use mcp_semantic_memory::consolidation::{self, Mode};
use mcp_semantic_memory::prelude::*;
#[cfg(feature = "mariadb")]
use mcp_semantic_memory_mariadb::MariaDbStore;
#[cfg(feature = "postgres")]
use mcp_semantic_memory_postgres::PostgresStore;
#[cfg(feature = "sqlite")]
use mcp_semantic_memory_sqlite::SqliteStore;
use rmcp::ServiceExt;
use rmcp::transport::stdio;

use crate::args::Command;

/// Does what the subcommand says, with the configuration it was started with.
pub async fn run(command: Command, config: Config) -> Result<()> {
  match command {
    Command::Migrate => {
      migrate(&config).await?;
      Ok(())
    }
    Command::Run => serve(config).await,
    Command::Consolidate { apply } => consolidate(config, apply).await,
    Command::Reembed => reembed(config).await,
  }
}

/// The embedder, the reranker and the store, as the configuration says.
struct Facades {
  embedder: Box<dyn Embedder>,
  reranker: Box<dyn Reranker>,
  store: Box<dyn Store>,
}

async fn facades(config: &Config) -> Result<Facades> {
  let embedder = OpenAiEmbedder::new(&config.embedding, config.timeout)?;
  let fingerprint = embedder.fingerprint().clone();
  let store: Box<dyn Store> = match config.dialect()? {
    #[cfg(feature = "sqlite")]
    Dialect::Sqlite => {
      Box::new(SqliteStore::connect(&config.database, fingerprint).await?)
    }
    #[cfg(feature = "postgres")]
    Dialect::Postgres => {
      Box::new(PostgresStore::connect(&config.database, fingerprint).await?)
    }
    #[cfg(feature = "mariadb")]
    Dialect::MariaDb => {
      Box::new(MariaDbStore::connect(&config.database, fingerprint).await?)
    }
    #[allow(
      unreachable_patterns,
      reason = "a build with every database has no dialect left over"
    )]
    dialect => bail!("this build was made without {dialect:?} support"),
  };
  let reranker: Box<dyn Reranker> = match &config.rerank.url {
    Some(url) => Box::new(JinaReranker::new(url, config.timeout)?),
    None => Box::new(NoReranker),
  };

  Ok(Facades {
    embedder: Box::new(embedder),
    reranker,
    store,
  })
}

async fn migrate(config: &Config) -> Result<Facades> {
  let facades = facades(config).await?;
  facades.store.migrate().await?;
  tracing::info!("the database is migrated");
  Ok(facades)
}

async fn serve(config: Config) -> Result<()> {
  let Facades {
    embedder,
    reranker,
    store,
  } = migrate(&config).await?;
  let service =
    Service::new(embedder, reranker, store, config.write, config.search)
      .with_consolidation(config.consolidation);

  let server = MemoryServer::new(service).checked().await?;
  let running = server
    .serve(stdio())
    .await
    .inspect_err(|error| tracing::error!(%error, "could not start serving"))?;
  running.waiting().await?;
  Ok(())
}

/// After the embedding model changed: every memory is embedded again. Like
/// `consolidate`, it does not wait for the database to be current, since
/// being current is what it is for.
async fn reembed(config: Config) -> Result<()> {
  let Facades {
    embedder, store, ..
  } = facades(&config).await?;
  let report =
    mcp_semantic_memory::reembed::reembed(&store, &embedder, &config.reembed)
      .await?;
  tracing::info!(embedded = report.embedded, "re-embedded the memories");
  write!(std::io::stdout().lock(), "{report}")?;
  Ok(())
}

/// The upkeep, from a timer. It only reports unless it is told to apply.
/// Unlike the startup checks, it does not wait for the database to be
/// current, except for the one that matters here: clustering memories whose
/// vectors come from different models means nothing.
async fn consolidate(config: Config, apply: bool) -> Result<()> {
  let Facades { store, .. } = facades(&config).await?;
  let health = store.health().await?;
  if health.memories_to_reembed > 0 {
    bail!(
      "{} active memories have no vector from the current embedding model. \
       Run `reembed` first.",
      health.memories_to_reembed
    );
  }

  let mode = if apply {
    Mode::Apply {
      only_if_needed: None,
    }
  } else {
    Mode::DryRun
  };
  let report = consolidation::consolidate(
    &store,
    &config.consolidation,
    config.search.half_life,
    mode,
  )
  .await?;
  tracing::info!(
    status = ?report.status,
    interests = report.interests.len(),
    forgotten = report.forgotten.len(),
    "consolidated"
  );
  write!(std::io::stdout().lock(), "{report}")?;
  Ok(())
}
