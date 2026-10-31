//! The SQLite store: sqlx with plain SQL, `sqlite-vec` for the vectors and
//! FTS5 for the keywords.
//!
//! Vectors go in and out as JSON text (`vec_f32` and `vec_to_json`), so no
//! driver-specific vector type is needed.
//!
//! The queries are the `sqlx::query*!` macros, which check them against the
//! schema while compiling (see `AGENTS.md` for how). The ones that touch the
//! vector tables are plain `sqlx::query*` calls, because the database the
//! macros check against cannot load `sqlite-vec`.

use std::str::FromStr;
use std::sync::Once;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mcp_semantic_memory::config::DatabaseConfig;
use mcp_semantic_memory::store::{
  Consolidated, Consolidation, Embedded, Forgotten, Health, MemoryText, Store,
  StoreError, Swapped, Write, WriteOutcome, keywords,
};
use mcp_semantic_memory::types::{
  Candidate, Event, EventId, Fingerprint, Memory, MemoryId, NewEvent, Vector,
  Version,
};
use sqlx::sqlite::{
  SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions,
};
use sqlx::{AssertSqlSafe, FromRow, SqliteConnection, SqlitePool};

type Result<T> = std::result::Result<T, StoreError>;

/// The most neighbours `sqlite-vec` will look for in one query.
const MAX_K: usize = 4096;

/// A row of the `memories` table, as the database has it.
#[derive(FromRow)]
struct MemoryRow {
  id: i64,
  content: String,
  content_hash: String,
  event_count: i64,
  last_event_at: String,
  interest_score: Option<f64>,
  forgotten_at: Option<String>,
  version: i64,
  created_at: String,
  updated_at: String,
}

impl TryFrom<MemoryRow> for Memory {
  type Error = StoreError;

  fn try_from(row: MemoryRow) -> Result<Self> {
    Ok(Self {
      id: MemoryId::new(row.id),
      content: row.content,
      content_hash: row.content_hash,
      event_count: row.event_count,
      last_event_at: time(&row.last_event_at)?,
      interest_score: row.interest_score.map(single),
      forgotten_at: row.forgotten_at.as_deref().map(time).transpose()?,
      version: Version::new(row.version),
      created_at: time(&row.created_at)?,
      updated_at: time(&row.updated_at)?,
    })
  }
}

/// A memory row and its distance from the vector that was searched for.
#[derive(FromRow)]
struct CandidateRow {
  #[sqlx(flatten)]
  memory: MemoryRow,
  distance: f64,
}

impl TryFrom<CandidateRow> for Candidate {
  type Error = StoreError;

  fn try_from(row: CandidateRow) -> Result<Self> {
    Ok(Self {
      memory: row.memory.try_into()?,
      distance: single(row.distance),
    })
  }
}

/// A memory row and its vector as JSON text.
#[derive(FromRow)]
struct EmbeddedRow {
  #[sqlx(flatten)]
  memory: MemoryRow,
  vector: String,
}

impl TryFrom<EmbeddedRow> for Embedded {
  type Error = StoreError;

  fn try_from(row: EmbeddedRow) -> Result<Self> {
    Ok(Self {
      memory: row.memory.try_into()?,
      vector: parse_vector(&row.vector)?,
    })
  }
}

/// A row of the `events` table, as the database has it.
struct EventRow {
  id: i64,
  memory_id: i64,
  content: String,
  metadata: String,
  memory_text: String,
  created_at: String,
}

impl TryFrom<EventRow> for Event {
  type Error = StoreError;

  fn try_from(row: EventRow) -> Result<Self> {
    Ok(Self {
      id: EventId::new(row.id),
      memory_id: MemoryId::new(row.memory_id),
      content: row.content,
      metadata: serde_json::from_str(&row.metadata).map_err(|error| {
        StoreError::Corrupt(format!(
          "an event has metadata that is not JSON: {error}"
        ))
      })?,
      memory_text: row.memory_text,
      created_at: time(&row.created_at)?,
    })
  }
}

/// Times are RFC 3339 text from the database clock.
fn time(text: &str) -> Result<DateTime<Utc>> {
  text
    .parse()
    .map_err(|_| StoreError::Corrupt(format!("{text:?} is not a time")))
}

#[allow(
  clippy::as_conversions,
  reason = "SQLite stores REAL as f64, and scores and distances are well \
            inside what f32 holds"
)]
fn single(number: f64) -> f32 {
  number as f32
}

/// A store on one SQLite database, which may be a file or in memory.
pub struct SqliteStore {
  pool: SqlitePool,
  fingerprint: Fingerprint,
}

impl SqliteStore {
  /// The file is created when it is missing. Vectors are stored with
  /// `fingerprint`, and its dimension sizes the vector tables.
  pub async fn connect(
    config: &DatabaseConfig,
    fingerprint: Fingerprint,
  ) -> Result<Self> {
    register_vector_extension();

    let options = SqliteConnectOptions::from_str(&config.url)
      .map_err(StoreError::backend("invalid SQLite connection URL"))?
      .create_if_missing(true)
      .foreign_keys(true)
      .journal_mode(SqliteJournalMode::Wal)
      .busy_timeout(config.busy_timeout);

    // NOTE: an in-memory database lives and dies with its connection, so
    // there has to be exactly one and it must never be closed
    let in_memory =
      config.url.contains(":memory:") || config.url.contains("mode=memory");
    let max_connections = if in_memory { 1 } else { config.max_connections };
    let pool = SqlitePoolOptions::new()
      .max_connections(max_connections)
      .min_connections(config.min_connections.min(max_connections))
      .acquire_timeout(config.acquire_timeout)
      .idle_timeout(None)
      .max_lifetime(None)
      .connect_with(options)
      .await
      .map_err(StoreError::backend("could not open the SQLite database"))?;

    Ok(Self { pool, fingerprint })
  }

  async fn write_new(
    &self,
    connection: &mut SqliteConnection,
    event: &NewEvent,
    memory: &MemoryText,
  ) -> Result<WriteOutcome> {
    if let Some(id) = holder_of(connection, &memory.hash).await? {
      return Ok(WriteOutcome::Duplicate(id));
    }

    let id = sqlx::query_scalar!(
      r#"INSERT INTO memories
           (content, content_hash, event_count, interest_score)
         VALUES (?1, ?2, 1, ?3)
         RETURNING id AS "id!: i64""#,
      memory.text,
      memory.hash,
      memory.interest_score,
    )
    .fetch_one(&mut *connection)
    .await?;
    let id = MemoryId::new(id);
    insert_event(connection, id, event, &memory.text).await?;
    self.put_vector(connection, id, &memory.vector).await?;
    Ok(WriteOutcome::Written(id))
  }

  async fn write_attach(
    &self,
    connection: &mut SqliteConnection,
    memory_id: MemoryId,
    version: Version,
    event: &NewEvent,
    memory: &MemoryText,
  ) -> Result<WriteOutcome> {
    if version_of(connection, memory_id).await? != version {
      return Ok(WriteOutcome::Conflict);
    }
    if let Some(id) =
      other_holder_of(connection, &memory.hash, memory_id).await?
    {
      return Ok(WriteOutcome::Duplicate(id));
    }

    sqlx::query!(
      "UPDATE memories SET
         content = ?2,
         content_hash = ?3,
         interest_score = ?4,
         event_count = event_count + 1,
         last_event_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
         forgotten_at = NULL,
         version = version + 1,
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
       WHERE id = ?1",
      memory_id.get(),
      memory.text,
      memory.hash,
      memory.interest_score,
    )
    .execute(&mut *connection)
    .await?;
    insert_event(connection, memory_id, event, &memory.text).await?;
    self
      .put_vector(connection, memory_id, &memory.vector)
      .await?;
    Ok(WriteOutcome::Written(memory_id))
  }

  async fn write_repeat(
    &self,
    connection: &mut SqliteConnection,
    memory_id: MemoryId,
    version: Version,
    event: &NewEvent,
  ) -> Result<WriteOutcome> {
    if version_of(connection, memory_id).await? != version {
      return Ok(WriteOutcome::Conflict);
    }

    let metadata = event.metadata.to_string();
    sqlx::query!(
      "INSERT INTO events (memory_id, content, metadata, memory_text)
       SELECT id, ?2, ?3, content FROM memories WHERE id = ?1",
      memory_id.get(),
      event.content,
      metadata,
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query!(
      "UPDATE memories SET
         event_count = event_count + 1,
         last_event_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
         forgotten_at = NULL,
         version = version + 1,
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
       WHERE id = ?1",
      memory_id.get(),
    )
    .execute(&mut *connection)
    .await?;
    Ok(WriteOutcome::Written(memory_id))
  }

  /// The vector index has no update, so the old vector goes and the new one
  /// takes its place.
  async fn put_vector(
    &self,
    connection: &mut SqliteConnection,
    memory_id: MemoryId,
    vector: &[f32],
  ) -> Result<()> {
    sqlx::query("DELETE FROM memory_vectors WHERE memory_id = ?1")
      .bind(memory_id.get())
      .execute(&mut *connection)
      .await?;
    sqlx::query(
      "INSERT INTO memory_vectors (memory_id, embedding, fingerprint)
       VALUES (?1, vec_f32(?2), ?3)",
    )
    .bind(memory_id.get())
    .bind(vector_json(vector)?)
    .bind(self.fingerprint.to_string())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::backend("could not store the vector"))?;
    Ok(())
  }

  /// A vector column needs its dimension up front, so the vector tables are
  /// made from code and not from the migration files.
  async fn create_vector_table(
    &self,
    connection: &mut SqliteConnection,
    table: &str,
    key: &str,
  ) -> Result<()> {
    let dimension = self.fingerprint.dimension;
    let create = format!(
      "CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(
         {key} INTEGER PRIMARY KEY,
         embedding FLOAT[{dimension}] distance_metric=cosine,
         +fingerprint TEXT
       )"
    );
    // The table names are the ones the callers pass and the dimension is a
    // number.
    sqlx::query(AssertSqlSafe(create))
      .execute(&mut *connection)
      .await
      .map_err(StoreError::backend("could not create a vector table"))?;
    Ok(())
  }
}

// NOTE: a static is the only way to run something once for the whole process,
// and SQLite keeps its list of extensions per process too
static REGISTER: Once = Once::new();

/// `sqlite-vec` is part of this binary and not a shared library, so the
/// binary stays static. Every connection opened after this has it.
#[allow(
  unsafe_code,
  reason = "SQLite takes extensions as C function pointers, and sqlite-vec \
            is linked into this binary"
)]
fn register_vector_extension() {
  use std::ffi::{c_char, c_int};

  use libsqlite3_sys::{sqlite3, sqlite3_api_routines};

  type EntryPoint = unsafe extern "C" fn(
    *mut sqlite3,
    *mut *mut c_char,
    *const sqlite3_api_routines,
  ) -> c_int;

  REGISTER.call_once(|| {
    // SAFETY: `sqlite3_vec_init` is an extension entry point, but the crate
    // declares it without arguments, so only the type of the pointer differs
    unsafe {
      libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute::<
        unsafe extern "C" fn(),
        EntryPoint,
      >(
        sqlite_vec::sqlite3_vec_init,
      )));
    }
  });
}

fn vector_json(vector: &[f32]) -> Result<String> {
  serde_json::to_string(vector)
    .map_err(StoreError::backend("could not write the vector as JSON"))
}

fn parse_vector(json: &str) -> Result<Vector> {
  serde_json::from_str(json).map_err(|error| {
    StoreError::Corrupt(format!("a vector is not a list of numbers: {error}"))
  })
}

fn limit_number(limit: usize) -> i64 {
  i64::try_from(limit).unwrap_or(i64::MAX)
}

fn count(number: i64) -> u64 {
  u64::try_from(number).unwrap_or(0)
}

/// The words of a text as an FTS5 query: each one quoted, so nothing in it
/// can be taken for query syntax, and any of them may match.
fn match_query(text: &str) -> Option<String> {
  let words = keywords(text);
  if words.is_empty() {
    return None;
  }
  Some(
    words
      .iter()
      .map(|word| format!("\"{word}\""))
      .collect::<Vec<_>>()
      .join(" OR "),
  )
}

/// The memory that already has this text.
async fn holder_of(
  connection: &mut SqliteConnection,
  hash: &str,
) -> Result<Option<MemoryId>> {
  let id = sqlx::query_scalar!(
    "SELECT id FROM memories WHERE content_hash = ?1",
    hash
  )
  .fetch_optional(&mut *connection)
  .await?;
  Ok(id.map(MemoryId::new))
}

/// The memory that already has this text, other than `memory_id`.
async fn other_holder_of(
  connection: &mut SqliteConnection,
  hash: &str,
  memory_id: MemoryId,
) -> Result<Option<MemoryId>> {
  let id = sqlx::query_scalar!(
    "SELECT id FROM memories WHERE content_hash = ?1 AND id != ?2",
    hash,
    memory_id.get(),
  )
  .fetch_optional(&mut *connection)
  .await?;
  Ok(id.map(MemoryId::new))
}

async fn version_of(
  connection: &mut SqliteConnection,
  memory_id: MemoryId,
) -> Result<Version> {
  sqlx::query_scalar!(
    "SELECT version FROM memories WHERE id = ?1",
    memory_id.get()
  )
  .fetch_optional(&mut *connection)
  .await?
  .map(Version::new)
  .ok_or(StoreError::MissingMemory(memory_id))
}

async fn insert_event(
  connection: &mut SqliteConnection,
  memory_id: MemoryId,
  event: &NewEvent,
  memory_text: &str,
) -> Result<()> {
  let metadata = event.metadata.to_string();
  sqlx::query!(
    "INSERT INTO events (memory_id, content, metadata, memory_text)
     VALUES (?1, ?2, ?3, ?4)",
    memory_id.get(),
    event.content,
    metadata,
    memory_text,
  )
  .execute(&mut *connection)
  .await?;
  Ok(())
}

#[async_trait]
impl Store for SqliteStore {
  async fn migrate(&self) -> Result<()> {
    sqlx::migrate!("./migrations")
      .run(&self.pool)
      .await
      .map_err(StoreError::backend("could not run the migrations"))?;

    let mut connection = self.pool.acquire().await?;
    for (table, key) in [
      ("memory_vectors", "memory_id"),
      ("interest_vectors", "interest_id"),
    ] {
      self
        .create_vector_table(&mut connection, table, key)
        .await?;
    }
    Ok(())
  }

  async fn nearest(
    &self,
    vector: &[f32],
    limit: usize,
    forgotten: Forgotten,
  ) -> Result<Vec<Candidate>> {
    if limit == 0 {
      return Ok(Vec::new());
    }

    // NOTE: the vector index cannot skip forgotten memories, so ask it for
    // enough neighbours that `limit` of them are left even if every
    // forgotten memory is nearer than all the others
    let wanted = match forgotten {
      Forgotten::Include => limit,
      Forgotten::Skip => {
        let forgotten = sqlx::query_scalar!(
          r#"SELECT count(*) AS "count!: i64"
             FROM memories WHERE forgotten_at IS NOT NULL"#
        )
        .fetch_one(&self.pool)
        .await?;
        limit.saturating_add(usize::try_from(forgotten).unwrap_or(0))
      }
    };
    let k = wanted.min(MAX_K);

    let rows: Vec<CandidateRow> = sqlx::query_as(
      "SELECT m.*, v.distance
       FROM (
         SELECT memory_id, distance FROM memory_vectors
         WHERE embedding MATCH vec_f32(?1) AND k = ?2
       ) v
       JOIN memories m ON m.id = v.memory_id
       WHERE ?3 OR m.forgotten_at IS NULL
       ORDER BY v.distance
       LIMIT ?4",
    )
    .bind(vector_json(vector)?)
    .bind(limit_number(k))
    .bind(forgotten == Forgotten::Include)
    .bind(limit_number(limit))
    .fetch_all(&self.pool)
    .await
    .map_err(StoreError::backend("could not search the vectors"))?;
    rows.into_iter().map(Candidate::try_from).collect()
  }

  async fn keyword(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
    let Some(query) = match_query(query) else {
      return Ok(Vec::new());
    };
    let limit = limit_number(limit);
    let rows = sqlx::query_as!(
      MemoryRow,
      "SELECT m.*
       FROM memories_fts
       JOIN memories m ON m.id = memories_fts.rowid
       WHERE memories_fts MATCH ?1 AND m.forgotten_at IS NULL
       ORDER BY memories_fts.rank
       LIMIT ?2",
      query,
      limit,
    )
    .fetch_all(&self.pool)
    .await
    .map_err(StoreError::backend("could not search the keywords"))?;
    rows.into_iter().map(Memory::try_from).collect()
  }

  async fn write(&self, write: Write) -> Result<WriteOutcome> {
    // NOTE: take the write lock at the start so two writers wait for each
    // other here instead of failing when one of them tries to upgrade
    let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

    let outcome = match write {
      Write::New { event, memory } => {
        self.write_new(&mut tx, &event, &memory).await?
      }
      Write::Attach {
        memory_id,
        version,
        event,
        memory,
      } => {
        self
          .write_attach(&mut tx, memory_id, version, &event, &memory)
          .await?
      }
      Write::Repeat {
        memory_id,
        version,
        event,
      } => {
        self
          .write_repeat(&mut tx, memory_id, version, &event)
          .await?
      }
    };

    // NOTE: anything but a write is rolled back when the transaction drops
    if matches!(outcome, WriteOutcome::Written(_)) {
      tx.commit().await?;
    }
    Ok(outcome)
  }

  async fn memory(&self, id: MemoryId) -> Result<Option<Memory>> {
    sqlx::query_as!(MemoryRow, "SELECT * FROM memories WHERE id = ?1", id.get())
      .fetch_optional(&self.pool)
      .await?
      .map(Memory::try_from)
      .transpose()
  }

  async fn events(
    &self,
    memory_id: MemoryId,
    limit: usize,
  ) -> Result<Vec<Event>> {
    let limit = limit_number(limit);
    let rows = sqlx::query_as!(
      EventRow,
      "SELECT * FROM events WHERE memory_id = ?1 ORDER BY id DESC LIMIT ?2",
      memory_id.get(),
      limit,
    )
    .fetch_all(&self.pool)
    .await?;
    rows.into_iter().map(Event::try_from).collect()
  }

  async fn interests(&self) -> Result<Vec<Vector>> {
    let vectors: Vec<String> = sqlx::query_scalar(
      "SELECT vec_to_json(v.embedding)
       FROM interests i
       JOIN interest_vectors v ON v.interest_id = i.id
       WHERE i.run_at = (SELECT max(run_at) FROM interests)
       ORDER BY i.id",
    )
    .fetch_all(&self.pool)
    .await?;
    vectors.iter().map(|json| parse_vector(json)).collect()
  }

  async fn health(&self) -> Result<Health> {
    let mut connection = self.pool.acquire().await?;
    health_of(&mut connection, &self.fingerprint).await
  }

  async fn sample(&self, limit: usize) -> Result<Vec<Embedded>> {
    let rows: Vec<EmbeddedRow> = sqlx::query_as(
      "SELECT m.*, vec_to_json(v.embedding) AS vector
       FROM memories m
       JOIN memory_vectors v ON v.memory_id = m.id
       WHERE m.forgotten_at IS NULL
       ORDER BY m.event_count DESC, m.last_event_at DESC, m.id
       LIMIT ?1",
    )
    .bind(limit_number(limit))
    .fetch_all(&self.pool)
    .await
    .map_err(StoreError::backend("could not read the sample"))?;
    rows.into_iter().map(Embedded::try_from).collect()
  }

  async fn active(
    &self,
    after: Option<MemoryId>,
    limit: usize,
  ) -> Result<Vec<Embedded>> {
    let rows: Vec<EmbeddedRow> = sqlx::query_as(
      "SELECT m.*, vec_to_json(v.embedding) AS vector
       FROM memories m
       JOIN memory_vectors v ON v.memory_id = m.id
       WHERE m.forgotten_at IS NULL AND m.id > ?1
       ORDER BY m.id
       LIMIT ?2",
    )
    .bind(after.map_or(i64::MIN, MemoryId::get))
    .bind(limit_number(limit))
    .fetch_all(&self.pool)
    .await
    .map_err(StoreError::backend("could not read the memories"))?;
    rows.into_iter().map(Embedded::try_from).collect()
  }

  async fn consolidate(
    &self,
    consolidation: Consolidation,
    only_if_needed: Option<Duration>,
  ) -> Result<Consolidated> {
    // NOTE: the write lock is the lock that only one instance can hold, and
    // it is taken before anything is read
    let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

    if let Some(max_age) = only_if_needed {
      let health = health_of(&mut tx, &self.fingerprint).await?;
      if !health.needs_consolidation(max_age) {
        return Ok(Consolidated::NotStale);
      }
    }

    // NOTE: the run time is a millisecond after the one before at the least,
    // or two runs in the same millisecond would count as one
    let run_at = sqlx::query_scalar!(
      r#"SELECT MAX(
           strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
           COALESCE(strftime(
             '%Y-%m-%dT%H:%M:%fZ',
             (SELECT max(run_at) FROM interests),
             '+0.001 seconds'
           ), '')
         ) AS "run_at!: String""#
    )
    .fetch_one(&mut *tx)
    .await?;
    for interest in &consolidation.interests {
      let id = sqlx::query_scalar!(
        r#"INSERT INTO interests (run_at, top_terms)
           VALUES (?1, ?2)
           RETURNING id AS "id!: i64""#,
        run_at,
        interest.top_terms,
      )
      .fetch_one(&mut *tx)
      .await?;
      sqlx::query(
        "INSERT INTO interest_vectors (interest_id, embedding, fingerprint)
         VALUES (?1, vec_f32(?2), ?3)",
      )
      .bind(id)
      .bind(vector_json(&interest.centre)?)
      .bind(self.fingerprint.to_string())
      .execute(&mut *tx)
      .await
      .map_err(StoreError::backend("could not store an interest"))?;
    }

    for verdict in &consolidation.verdicts {
      let score = f64::from(verdict.interest_score);
      sqlx::query!(
        "UPDATE memories SET
           interest_score = ?3,
           forgotten_at = CASE WHEN ?4 THEN ?5 ELSE forgotten_at END
         WHERE id = ?1 AND version = ?2",
        verdict.memory_id.get(),
        verdict.version.get(),
        score,
        verdict.forget,
        run_at,
      )
      .execute(&mut *tx)
      .await?;
    }

    tx.commit().await?;
    Ok(Consolidated::Written)
  }

  async fn begin_reembed(&self) -> Result<()> {
    let mut connection = self.pool.acquire().await?;
    self
      .create_vector_table(&mut connection, "memory_vectors_next", "memory_id")
      .await?;
    let kept: i64 = sqlx::query_scalar(
      "SELECT count(*) FROM memory_vectors_next WHERE fingerprint = ?1",
    )
    .bind(self.fingerprint.to_string())
    .fetch_one(&mut *connection)
    .await?;
    if kept == 0 {
      sqlx::query("DROP TABLE memory_vectors_next")
        .execute(&mut *connection)
        .await?;
      self
        .create_vector_table(
          &mut connection,
          "memory_vectors_next",
          "memory_id",
        )
        .await?;
    }
    Ok(())
  }

  async fn unembedded(&self, limit: usize) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = sqlx::query_as(
      "SELECT m.*
       FROM memories m
       LEFT JOIN memory_vectors_next v ON v.memory_id = m.id
       WHERE v.memory_id IS NULL OR v.fingerprint != ?1
       ORDER BY m.id
       LIMIT ?2",
    )
    .bind(self.fingerprint.to_string())
    .bind(limit_number(limit))
    .fetch_all(&self.pool)
    .await
    .map_err(StoreError::backend("could not read the memories to embed"))?;
    rows.into_iter().map(Memory::try_from).collect()
  }

  async fn put_reembedded(
    &self,
    vectors: Vec<(MemoryId, Vector)>,
  ) -> Result<()> {
    let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
    for (memory_id, vector) in vectors {
      sqlx::query("DELETE FROM memory_vectors_next WHERE memory_id = ?1")
        .bind(memory_id.get())
        .execute(&mut *tx)
        .await?;
      sqlx::query(
        "INSERT INTO memory_vectors_next (memory_id, embedding, fingerprint)
         VALUES (?1, vec_f32(?2), ?3)",
      )
      .bind(memory_id.get())
      .bind(vector_json(&vector)?)
      .bind(self.fingerprint.to_string())
      .execute(&mut *tx)
      .await
      .map_err(StoreError::backend("could not store a new vector"))?;
    }
    tx.commit().await?;
    Ok(())
  }

  async fn swap_vectors(&self) -> Result<Swapped> {
    // NOTE: the write lock, so no memory can be written between the check
    // that none is missing and the swap
    let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

    let missing: i64 = sqlx::query_scalar(
      "SELECT count(*)
       FROM memories m
       LEFT JOIN memory_vectors_next v ON v.memory_id = m.id
       WHERE v.memory_id IS NULL OR v.fingerprint != ?1",
    )
    .bind(self.fingerprint.to_string())
    .fetch_one(&mut *tx)
    .await?;
    if missing > 0 {
      return Ok(Swapped::Missing);
    }

    // NOTE: a `vec0` table cannot be renamed. The queries on it fail
    // afterwards, so the vectors are copied into a fresh table instead
    for statement in
      ["DROP TABLE memory_vectors", "DROP TABLE interest_vectors"]
    {
      sqlx::query(statement)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::backend("could not drop a vector table"))?;
    }
    for (table, key) in [
      ("memory_vectors", "memory_id"),
      ("interest_vectors", "interest_id"),
    ] {
      self.create_vector_table(&mut tx, table, key).await?;
    }
    for statement in [
      "INSERT INTO memory_vectors (memory_id, embedding, fingerprint)
       SELECT memory_id, embedding, fingerprint FROM memory_vectors_next",
      "DROP TABLE memory_vectors_next",
    ] {
      sqlx::query(statement)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::backend("could not copy the new vectors"))?;
    }
    tx.commit().await?;
    Ok(Swapped::Done)
  }
}

/// What the startup checks need, read on one connection so a transaction can
/// ask too.
async fn health_of(
  connection: &mut SqliteConnection,
  fingerprint: &Fingerprint,
) -> Result<Health> {
  let fingerprint = fingerprint.to_string();

  let memories: i64 = sqlx::query_scalar(
    "SELECT count(*)
     FROM memories m
     LEFT JOIN memory_vectors v ON v.memory_id = m.id
     WHERE m.forgotten_at IS NULL
       AND (v.fingerprint IS NULL OR v.fingerprint != ?1)",
  )
  .bind(&fingerprint)
  .fetch_one(&mut *connection)
  .await?;

  let interests: i64 = sqlx::query_scalar(
    "SELECT count(*)
     FROM interests i
     LEFT JOIN interest_vectors v ON v.interest_id = i.id
     WHERE i.run_at = (SELECT max(run_at) FROM interests)
       AND (v.fingerprint IS NULL OR v.fingerprint != ?1)",
  )
  .bind(&fingerprint)
  .fetch_one(&mut *connection)
  .await?;

  // NOTE: the times are the database's, and the newest event stands in for
  // the time now, so a quiet spell is not a reason to consolidate
  let seconds = sqlx::query_scalar!(
    r#"SELECT (julianday(newest) - julianday(COALESCE(latest, oldest)))
         * 86400.0 AS "seconds?: f64"
       FROM (
         SELECT max(last_event_at) AS newest, min(created_at) AS oldest
         FROM memories WHERE forgotten_at IS NULL
       ), (SELECT max(run_at) AS latest FROM interests)"#
  )
  .fetch_one(&mut *connection)
  .await?;
  let since_consolidation = seconds
    .map(|seconds| {
      Duration::try_from_secs_f64(seconds.max(0.0))
        .map_err(|_| StoreError::Corrupt(format!("{seconds} is not an age")))
    })
    .transpose()?;

  Ok(Health {
    memories_to_reembed: count(memories),
    interests_to_rebuild: count(interests),
    since_consolidation,
  })
}

#[cfg(test)]
mod tests {
  use serde_json::Value;

  use super::*;

  async fn store() -> SqliteStore {
    let database = DatabaseConfig {
      url: "sqlite::memory:".to_owned(),
      ..DatabaseConfig::default()
    };
    let store = SqliteStore::connect(
      &database,
      Fingerprint {
        model: "test".to_owned(),
        dimension: 2,
      },
    )
    .await
    .unwrap();
    store.migrate().await.unwrap();
    store
  }

  async fn add(store: &SqliteStore, text: &str, vector: Vector) -> MemoryId {
    let write = Write::New {
      event: NewEvent {
        content: text.to_owned(),
        metadata: Value::Null,
      },
      memory: MemoryText {
        text: text.to_owned(),
        hash: format!("hash of {text}"),
        vector,
        interest_score: None,
      },
    };
    match store.write(write).await.unwrap() {
      WriteOutcome::Written(id) => id,
      other => panic!("expected a write, got {other:?}"),
    }
  }

  async fn forget(store: &SqliteStore, ids: &[MemoryId]) {
    for id in ids {
      sqlx::query(
        "UPDATE memories
         SET forgotten_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
      )
      .bind(id.get())
      .execute(&store.pool)
      .await
      .unwrap();
    }
  }

  #[test]
  fn the_query_words_are_quoted_and_joined() {
    assert_eq!(
      match_query("Cat, cat! sat?").as_deref(),
      Some("\"cat\" OR \"sat\"")
    );
    assert_eq!(match_query("  ... \" ' ( ) "), None);
  }

  #[tokio::test]
  async fn nearest_skips_forgotten_memories_unless_asked() {
    let store = store().await;
    let first = add(&store, "first", vec![1.0, 0.0]).await;
    let second = add(&store, "second", vec![0.9, 0.1]).await;
    let third = add(&store, "third", vec![0.0, 1.0]).await;
    forget(&store, &[first, second]).await;

    let found = store
      .nearest(&[1.0, 0.0], 1, Forgotten::Skip)
      .await
      .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].memory.id, third);

    let found = store
      .nearest(&[1.0, 0.0], 1, Forgotten::Include)
      .await
      .unwrap();
    assert_eq!(found[0].memory.id, first);
    assert!(found[0].memory.forgotten_at.is_some());
  }

  #[tokio::test]
  async fn keyword_skips_forgotten_memories() {
    let store = store().await;
    let id = add(&store, "cat sleeps", vec![1.0, 0.0]).await;
    assert_eq!(store.keyword("cat", 10).await.unwrap().len(), 1);
    forget(&store, &[id]).await;
    assert!(store.keyword("cat", 10).await.unwrap().is_empty());
  }

  #[tokio::test]
  async fn a_vector_of_the_wrong_length_is_refused_and_writes_nothing() {
    let store = store().await;
    let write = Write::New {
      event: NewEvent {
        content: "cat".to_owned(),
        metadata: Value::Null,
      },
      memory: MemoryText {
        text: "cat".to_owned(),
        hash: "cat".to_owned(),
        vector: vec![1.0, 0.0, 0.0],
        interest_score: None,
      },
    };
    assert!(store.write(write).await.is_err());
    assert!(store.keyword("cat", 10).await.unwrap().is_empty());
  }

  const DAY: Duration = Duration::from_secs(60 * 60 * 24);

  /// Puts the vector of a memory back as another embedding model made it.
  async fn remake_vector(store: &SqliteStore, id: MemoryId, fingerprint: &str) {
    sqlx::query("DELETE FROM memory_vectors WHERE memory_id = ?1")
      .bind(id.get())
      .execute(&store.pool)
      .await
      .unwrap();
    sqlx::query(
      "INSERT INTO memory_vectors (memory_id, embedding, fingerprint)
       VALUES (?1, vec_f32('[0.0, 1.0]'), ?2)",
    )
    .bind(id.get())
    .bind(fingerprint)
    .execute(&store.pool)
    .await
    .unwrap();
  }

  /// The time `ago` before now, as SQLite writes it, for example `-1 days`.
  async fn when(store: &SqliteStore, ago: &str) -> String {
    sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1)")
      .bind(ago)
      .fetch_one(&store.pool)
      .await
      .unwrap()
  }

  /// An interest of the run at `run_at`. Its vector is left out when there is
  /// no fingerprint.
  async fn add_interest(
    store: &SqliteStore,
    run_at: &str,
    fingerprint: Option<&str>,
  ) {
    let id: i64 = sqlx::query_scalar(
      "INSERT INTO interests (run_at, top_terms)
       VALUES (?1, 'cat')
       RETURNING id",
    )
    .bind(run_at)
    .fetch_one(&store.pool)
    .await
    .unwrap();
    if let Some(fingerprint) = fingerprint {
      sqlx::query(
        "INSERT INTO interest_vectors (interest_id, embedding, fingerprint)
         VALUES (?1, vec_f32('[1.0, 0.0]'), ?2)",
      )
      .bind(id)
      .bind(fingerprint)
      .execute(&store.pool)
      .await
      .unwrap();
    }
  }

  async fn age_memories(store: &SqliteStore, ago: &str) {
    sqlx::query(
      "UPDATE memories
       SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1)",
    )
    .bind(ago)
    .execute(&store.pool)
    .await
    .unwrap();
  }

  #[tokio::test]
  async fn an_empty_store_is_healthy() {
    let health = store().await.health().await.unwrap();
    assert_eq!(
      health,
      Health {
        memories_to_reembed: 0,
        interests_to_rebuild: 0,
        since_consolidation: None,
      }
    );
  }

  #[tokio::test]
  async fn memories_made_by_another_model_need_a_reembed() {
    let store = store().await;
    add(&store, "first", vec![1.0, 0.0]).await;
    let second = add(&store, "second", vec![0.0, 1.0]).await;
    assert_eq!(store.health().await.unwrap().memories_to_reembed, 0);

    remake_vector(&store, second, "other/2/cosine").await;
    assert_eq!(store.health().await.unwrap().memories_to_reembed, 1);

    forget(&store, &[second]).await;
    assert_eq!(store.health().await.unwrap().memories_to_reembed, 0);
  }

  #[tokio::test]
  async fn a_memory_without_a_vector_needs_a_reembed() {
    let store = store().await;
    let id = add(&store, "first", vec![1.0, 0.0]).await;
    sqlx::query("DELETE FROM memory_vectors WHERE memory_id = ?1")
      .bind(id.get())
      .execute(&store.pool)
      .await
      .unwrap();
    assert_eq!(store.health().await.unwrap().memories_to_reembed, 1);
  }

  #[tokio::test]
  async fn only_the_latest_interests_need_vectors() {
    let store = store().await;
    let old = when(&store, "-2 days").await;
    let middle = when(&store, "-1 days").await;
    let latest = when(&store, "-1 minutes").await;
    add_interest(&store, &old, Some("other/2/cosine")).await;
    add_interest(&store, &middle, Some("test/2/cosine")).await;
    assert_eq!(store.health().await.unwrap().interests_to_rebuild, 0);

    add_interest(&store, &latest, Some("other/2/cosine")).await;
    add_interest(&store, &latest, None).await;
    add_interest(&store, &latest, Some("test/2/cosine")).await;
    assert_eq!(store.health().await.unwrap().interests_to_rebuild, 2);
  }

  #[tokio::test]
  async fn the_age_counts_from_the_oldest_memory_until_a_consolidation() {
    let store = store().await;
    add(&store, "first", vec![1.0, 0.0]).await;
    let fresh = store.health().await.unwrap().since_consolidation.unwrap();
    assert!(fresh < Duration::from_secs(60), "{fresh:?}");

    age_memories(&store, "-3 days").await;
    let old = store.health().await.unwrap().since_consolidation.unwrap();
    assert!(about(old, DAY * 3), "{old:?}");

    let run_at = when(&store, "-1 days").await;
    add_interest(&store, &run_at, Some("test/2/cosine")).await;
    let since = store.health().await.unwrap().since_consolidation.unwrap();
    assert!(about(since, DAY), "{since:?}");
  }

  /// Whether `age` is `expected`, give or take the little time the test took.
  fn about(age: Duration, expected: Duration) -> bool {
    age.abs_diff(expected) < Duration::from_secs(60)
  }

  #[tokio::test]
  async fn forgotten_memories_have_nothing_to_consolidate() {
    let store = store().await;
    let id = add(&store, "first", vec![1.0, 0.0]).await;
    forget(&store, &[id]).await;
    assert_eq!(store.health().await.unwrap().since_consolidation, None);
  }
}
