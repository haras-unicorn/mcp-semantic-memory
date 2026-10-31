//! The configuration: an optional file, then the environment, which wins.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use config::builder::{ConfigBuilder, DefaultState};
use config::{Environment, File, FileFormat};
use serde::Deserialize;

// NOTE: the prefix and the file name are the program name, and the config
// crate takes them as strings, so they have to live in constants
const ENV_PREFIX: &str = "MCP_SEMANTIC_MEMORY";
const DEFAULT_FILE: &str = "mcp-semantic-memory.toml";

const DAY_SECS: u64 = 86_400;

const fn days(count: u64) -> Duration {
  Duration::from_secs(count.saturating_mul(DAY_SECS))
}

/// Which database the connection URL points at. The URL scheme decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
  /// `postgres:` or `postgresql:`.
  Postgres,
  /// `mariadb:` or `mysql:`.
  MariaDb,
  /// `sqlite:`.
  Sqlite,
}

impl Dialect {
  /// Fails for a scheme that is none of the dialects. Whether a build has the
  /// dialect is for the binary to say, since the stores are crates of their
  /// own.
  ///
  /// ```
  /// use mcp_semantic_memory::config::Dialect;
  ///
  /// let dialect = Dialect::from_url("sqlite::memory:").unwrap();
  /// assert_eq!(dialect, Dialect::Sqlite);
  /// assert!(Dialect::from_url("redis://host").is_err());
  /// ```
  pub fn from_url(url: &str) -> Result<Self> {
    match url.split_once(':').map(|(scheme, _)| scheme) {
      Some("postgres" | "postgresql") => Ok(Self::Postgres),
      Some("mysql" | "mariadb") => Ok(Self::MariaDb),
      Some("sqlite") => Ok(Self::Sqlite),
      _ => bail!(
        "the connection URL must start with postgres:, mariadb:, mysql: or \
         sqlite:"
      ),
    }
  }
}

/// Where the memories are kept.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
  /// Required. Its scheme picks the dialect.
  pub url: String,
  /// The most connections one instance opens. An in-memory SQLite database
  /// always gets one, because it lives and dies with its connection.
  pub max_connections: u32,
  /// The connections an instance keeps open while idle, so the first calls
  /// after a quiet spell do not wait for new ones. No more than
  /// `max_connections`.
  pub min_connections: u32,
  /// How long a call waits for a connection, new or free, before it fails.
  /// When a database goes away, as in a failover, calls fail after this long
  /// and the client tries again - an instance does not retry by itself.
  #[serde(with = "humantime_serde")]
  pub acquire_timeout: Duration,
  /// How long a statement waits for a lock before it fails. SQLite only.
  #[serde(with = "humantime_serde")]
  pub busy_timeout: Duration,
}

impl DatabaseConfig {
  fn validate(&self) -> Result<()> {
    ensure!(
      self.max_connections > 0,
      "database.max_connections must be more than zero"
    );
    ensure!(
      self.min_connections <= self.max_connections,
      "database.min_connections must be no more than max_connections"
    );
    ensure!(
      !self.acquire_timeout.is_zero(),
      "database.acquire_timeout must be more than zero"
    );
    Ok(())
  }
}

impl Default for DatabaseConfig {
  fn default() -> Self {
    Self {
      url: String::new(),
      max_connections: 4,
      min_connections: 0,
      acquire_timeout: Duration::from_secs(10),
      busy_timeout: Duration::from_secs(10),
    }
  }
}

/// The embedding server and what the model needs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
  /// Required. The address of the OpenAI-compatible server, without a path,
  /// like `http://localhost:8080`.
  pub url: String,
  /// Required. A label for the model. The server cannot learn it from the
  /// embedding server, so it comes from here. It goes into the embedding
  /// fingerprint.
  pub model: String,
  /// Required.
  pub dimension: usize,
  /// Text put in front of a query before it is embedded. Some models need it.
  pub query_prefix: String,
  /// Text put in front of a memory before it is embedded. Some models need it.
  pub document_prefix: String,
}

/// The rerank server.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RerankConfig {
  /// The address of the Jina-style rerank server, without a path. Without it
  /// nothing is reranked.
  pub url: Option<String>,
}

/// The write path.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WriteConfig {
  /// The longest event or memory text the store accepts.
  pub max_text_chars: usize,
  /// The most candidates the store shows the model.
  pub max_candidates: usize,
  /// How far (cosine) a memory may be from an event to count as a candidate.
  pub max_distance: f32,
}

impl Default for WriteConfig {
  fn default() -> Self {
    Self {
      max_text_chars: 2_000,
      max_candidates: 5,
      max_distance: 0.25,
    }
  }
}

/// Search and recall.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
  /// How many results `memory_search` returns when the call has no `limit`.
  pub limit: usize,
  /// How many memories each of the vector and keyword searches fetch.
  pub candidates: usize,
  /// How many events `memory_recall` returns when asked for them.
  pub max_events: usize,
  /// How many of the best candidates go through the reranker.
  pub rerank_candidates: usize,
  /// How much the vector ranking counts in the fusion.
  pub vector_weight: f32,
  /// How much the keyword ranking counts in the fusion.
  pub keyword_weight: f32,
  /// How much recent reinforcement lifts a memory.
  pub frecency_weight: f32,
  /// How much closeness to the interests lifts a memory.
  pub interest_weight: f32,
  /// The constant in reciprocal rank fusion. Higher flattens the ranks.
  pub rank_constant: f32,
  /// How long until a memory's recency counts half as much. More than zero.
  #[serde(with = "humantime_serde")]
  pub half_life: Duration,
}

impl Default for SearchConfig {
  fn default() -> Self {
    Self {
      limit: 10,
      candidates: 40,
      max_events: 50,
      rerank_candidates: 20,
      vector_weight: 1.0,
      keyword_weight: 0.5,
      frecency_weight: 0.1,
      interest_weight: 0.1,
      rank_constant: 60.0,
      half_life: days(30),
    }
  }
}

/// The upkeep run that finds the interests and forgets what faded.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ConsolidationConfig {
  /// How many of the most weighty memories are clustered.
  pub sample_size: usize,
  /// The base of the logarithm that turns the memory count into `k`.
  pub k_base: f32,
  /// The fewest interests.
  pub k_min: usize,
  /// The most interests.
  pub k_max: usize,
  /// How old a memory must be before it can be forgotten.
  #[serde(with = "humantime_serde")]
  pub grace: Duration,
  /// A memory with less frecency than this may be forgotten.
  pub forget_below_frecency: f32,
  /// A memory with less interest than this may be forgotten.
  pub forget_below_interest: f32,
  /// `run` refuses to serve when the newest event is more than this ahead of
  /// the last consolidation.
  #[serde(with = "humantime_serde")]
  pub max_age: Duration,
  /// Whether the `memory_consolidate` tool is offered, for a deployment with
  /// no timer to run `consolidate`. Leave it on for a single binary on one
  /// SQLite file, and turn it off where a timer does the job.
  pub has_tool: bool,
  /// The most rounds k-means runs. It stops sooner when nothing moves.
  pub iterations: usize,
  /// How many words name an interest.
  pub top_terms: usize,
  /// How many memories are read from the database at a time.
  pub batch_size: usize,
}

impl Default for ConsolidationConfig {
  fn default() -> Self {
    Self {
      sample_size: 2_000,
      k_base: std::f32::consts::E,
      k_min: 2,
      k_max: 16,
      grace: days(14),
      forget_below_frecency: 0.1,
      forget_below_interest: 0.3,
      max_age: days(14),
      has_tool: true,
      iterations: 50,
      top_terms: 5,
      batch_size: 500,
    }
  }
}

impl ConsolidationConfig {
  fn validate(&self) -> Result<()> {
    ensure!(
      self.k_base > 1.0,
      "consolidation.k_base must be more than one"
    );
    ensure!(
      self.k_min > 0 && self.k_min <= self.k_max,
      "consolidation.k_min must be at least one and no more than k_max"
    );
    ensure!(
      self.sample_size > 0,
      "consolidation.sample_size must be more than zero"
    );
    ensure!(
      self.batch_size > 0,
      "consolidation.batch_size must be more than zero"
    );
    Ok(())
  }
}

/// The run that embeds every memory again after the embedding model changed.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReembedConfig {
  /// How many memories are sent to the embedding server at a time.
  pub batch_size: usize,
}

impl Default for ReembedConfig {
  fn default() -> Self {
    Self { batch_size: 32 }
  }
}

impl ReembedConfig {
  fn validate(&self) -> Result<()> {
    ensure!(
      self.batch_size > 0,
      "reembed.batch_size must be more than zero"
    );
    Ok(())
  }
}

/// Read from an optional file and then from the environment, which wins.
/// Environment variables start with `MCP_SEMANTIC_MEMORY__` and use `__` to go
/// into a section, so `search.limit` is
/// `MCP_SEMANTIC_MEMORY__SEARCH__LIMIT`. The defaults for the numbers are
/// placeholders until they are tuned on real data.
///
/// Required values default to empty or zero and `validate` rejects them,
/// instead of the types leaving them out. That way one error names the key
/// and its variable, which serde's "missing field" would not.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
  /// Where the memories are kept.
  pub database: DatabaseConfig,
  /// The embedding server and what the model needs.
  pub embedding: EmbeddingConfig,
  /// The rerank server.
  pub rerank: RerankConfig,
  /// For the requests to the embedding and rerank servers.
  #[serde(with = "humantime_serde")]
  pub timeout: Duration,
  /// The write path.
  pub write: WriteConfig,
  /// Search and recall.
  pub search: SearchConfig,
  /// The upkeep run.
  pub consolidation: ConsolidationConfig,
  /// The run after the embedding model changed.
  pub reembed: ReembedConfig,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      database: DatabaseConfig::default(),
      embedding: EmbeddingConfig::default(),
      rerank: RerankConfig::default(),
      timeout: Duration::from_secs(30),
      write: WriteConfig::default(),
      search: SearchConfig::default(),
      consolidation: ConsolidationConfig::default(),
      reembed: ReembedConfig::default(),
    }
  }
}

impl Config {
  /// `file` is the path from `--config`, which must exist. It may be TOML, YAML
  /// or JSON, picked by its extension. Without it `mcp-semantic-memory.toml` in
  /// the current directory is read if it is there.
  pub fn load(file: Option<&Path>) -> Result<Self> {
    let builder = config::Config::builder();
    let builder = match file {
      Some(file) => builder.add_source(File::from(file)),
      None => builder
        .add_source(File::new(DEFAULT_FILE, FileFormat::Toml).required(false)),
    };
    Self::build(builder, environment())
  }

  /// Which database the URL points at.
  pub fn dialect(&self) -> Result<Dialect> {
    Dialect::from_url(&self.database.url)
  }

  fn build(
    builder: ConfigBuilder<DefaultState>,
    environment: Environment,
  ) -> Result<Self> {
    let mut config: Self = builder
      .add_source(environment)
      .build()
      .context("could not read the configuration")?
      .try_deserialize()
      .context("the configuration is not valid")?;

    config.rerank.url = config
      .rerank
      .url
      .take()
      .filter(|url| !url.trim().is_empty());
    config.validate()?;
    Ok(config)
  }

  fn validate(&self) -> Result<()> {
    required("database.url", &self.database.url)?;
    self.dialect()?;
    self.database.validate()?;
    required("embedding.url", &self.embedding.url)?;
    required("embedding.model", &self.embedding.model)?;
    ensure!(
      self.embedding.dimension > 0,
      "embedding.dimension is required ({})",
      variable("embedding.dimension")
    );
    ensure!(
      !self.search.half_life.is_zero(),
      "search.half_life must be more than zero"
    );
    self.consolidation.validate()?;
    self.reembed.validate()?;
    Ok(())
  }
}

fn variable(key: &str) -> String {
  format!("{ENV_PREFIX}__{}", key.replace('.', "__").to_uppercase())
}

fn required(key: &str, value: &str) -> Result<()> {
  ensure!(
    !value.trim().is_empty(),
    "{key} is required ({})",
    variable(key)
  );
  Ok(())
}

fn environment() -> Environment {
  Environment::with_prefix(ENV_PREFIX)
    .separator("__")
    .try_parsing(true)
    .ignore_empty(true)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn from(file: &str, env: &[(&str, &str)]) -> Result<Config> {
    let builder = config::Config::builder()
      .add_source(File::from_str(file, FileFormat::Toml));
    let environment = environment().source(Some(
      env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect(),
    ));
    Config::build(builder, environment)
  }

  const REQUIRED: [(&str, &str); 4] = [
    ("MCP_SEMANTIC_MEMORY__DATABASE__URL", "sqlite::memory:"),
    (
      "MCP_SEMANTIC_MEMORY__EMBEDDING__URL",
      "http://localhost:8080",
    ),
    ("MCP_SEMANTIC_MEMORY__EMBEDDING__MODEL", "test"),
    ("MCP_SEMANTIC_MEMORY__EMBEDDING__DIMENSION", "768"),
  ];

  fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("tests/fixtures")
      .join(name)
  }

  #[test]
  fn the_required_values_are_enough() {
    let config = from("", &REQUIRED).unwrap();
    assert_eq!(config.database.url, "sqlite::memory:");
    assert_eq!(config.embedding.dimension, 768);
    assert_eq!(config.dialect().unwrap(), Dialect::Sqlite);
    assert_eq!(config.search.limit, SearchConfig::default().limit);
    assert!(config.rerank.url.is_none());
  }

  #[test]
  fn a_value_in_a_section_comes_from_a_double_underscore() {
    let mut env = REQUIRED.to_vec();
    env.push(("MCP_SEMANTIC_MEMORY__SEARCH__LIMIT", "3"));
    env.push(("MCP_SEMANTIC_MEMORY__SEARCH__HALF_LIFE", "7d"));
    env.push(("MCP_SEMANTIC_MEMORY__DATABASE__BUSY_TIMEOUT", "500ms"));
    let config = from("", &env).unwrap();
    assert_eq!(config.search.limit, 3);
    assert_eq!(config.search.half_life, days(7));
    assert_eq!(config.database.busy_timeout, Duration::from_millis(500));
  }

  #[test]
  fn the_environment_wins_over_the_file() {
    let file = r#"
      timeout = "5s"

      [database]
      url = "sqlite::memory:"
      max_connections = 2

      [embedding]
      url = "http://from-file"
      model = "file"
      dimension = 4
    "#;
    let config = from(
      file,
      &[
        ("MCP_SEMANTIC_MEMORY__EMBEDDING__URL", "http://from-env"),
        ("MCP_SEMANTIC_MEMORY__TIMEOUT", "1m"),
      ],
    )
    .unwrap();
    assert_eq!(config.embedding.url, "http://from-env");
    assert_eq!(config.embedding.model, "file");
    assert_eq!(config.database.max_connections, 2);
    assert_eq!(config.timeout, Duration::from_secs(60));
  }

  #[test]
  fn what_is_missing_is_named_with_its_variable() {
    let error = from("", &[]).unwrap_err().to_string();
    assert!(error.contains("database.url"), "{error}");
    assert!(
      error.contains("MCP_SEMANTIC_MEMORY__DATABASE__URL"),
      "{error}"
    );

    let error = from("", &REQUIRED[..3]).unwrap_err().to_string();
    assert!(error.contains("embedding.dimension"), "{error}");
  }

  #[test]
  fn an_empty_rerank_url_means_no_reranking() {
    let mut env = REQUIRED.to_vec();
    env.push(("MCP_SEMANTIC_MEMORY__RERANK__URL", "  "));
    assert!(from("", &env).unwrap().rerank.url.is_none());
  }

  #[test]
  fn a_bad_duration_is_an_error() {
    let mut env = REQUIRED.to_vec();
    env.push(("MCP_SEMANTIC_MEMORY__SEARCH__HALF_LIFE", "soon"));
    assert!(from("", &env).is_err());
  }

  #[test]
  fn a_half_life_of_zero_is_an_error() {
    let mut env = REQUIRED.to_vec();
    env.push(("MCP_SEMANTIC_MEMORY__SEARCH__HALF_LIFE", "0s"));
    let error = from("", &env).unwrap_err().to_string();
    assert!(error.contains("search.half_life"), "{error}");
  }

  #[test]
  fn more_minimum_connections_than_maximum_is_an_error() {
    let mut env = REQUIRED.to_vec();
    env.push(("MCP_SEMANTIC_MEMORY__DATABASE__MIN_CONNECTIONS", "5"));
    let error = from("", &env).unwrap_err().to_string();
    assert!(error.contains("database.min_connections"), "{error}");
  }

  #[test]
  fn a_reembed_batch_of_zero_is_an_error() {
    let mut env = REQUIRED.to_vec();
    env.push(("MCP_SEMANTIC_MEMORY__REEMBED__BATCH_SIZE", "0"));
    let error = from("", &env).unwrap_err().to_string();
    assert!(error.contains("reembed.batch_size"), "{error}");
  }

  #[test]
  fn a_file_given_by_path_is_read() {
    let config = Config::load(Some(&fixture("config.toml"))).unwrap();
    assert_eq!(config.database.url, "sqlite::memory:");
    assert_eq!(config.embedding.model, "fixture");
    assert_eq!(config.embedding.dimension, 4);
  }

  #[test]
  fn a_file_given_by_path_must_exist() {
    assert!(Config::load(Some(&fixture("missing.toml"))).is_err());
  }

  #[test]
  fn a_missing_default_file_is_not_an_error_by_itself() {
    let error = Config::load(None).unwrap_err().to_string();
    assert!(error.contains("database.url"), "{error}");
  }

  #[test]
  fn dialect_follows_the_scheme() {
    assert_eq!(
      Dialect::from_url("postgres://u:p@host/db").unwrap(),
      Dialect::Postgres
    );
    assert_eq!(
      Dialect::from_url("postgresql://host/db").unwrap(),
      Dialect::Postgres
    );
    assert_eq!(
      Dialect::from_url("mariadb://host/db").unwrap(),
      Dialect::MariaDb
    );
    assert_eq!(
      Dialect::from_url("mysql://host/db").unwrap(),
      Dialect::MariaDb
    );
    assert_eq!(
      Dialect::from_url("sqlite::memory:").unwrap(),
      Dialect::Sqlite
    );
    assert_eq!(
      Dialect::from_url("sqlite:///var/lib/memory.db").unwrap(),
      Dialect::Sqlite
    );
  }

  #[test]
  fn unknown_schemes_are_errors() {
    assert!(Dialect::from_url("redis://host").is_err());
    assert!(Dialect::from_url("nonsense").is_err());
    assert!(Dialect::from_url("").is_err());
  }
}
