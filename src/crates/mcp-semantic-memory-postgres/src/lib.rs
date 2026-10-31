//! The Postgres store: sqlx with plain SQL, `pgvector` for the vectors and
//! full-text search for the keywords.
//!
//! Vectors go in and out as text (`$1::text::vector`), so no driver-specific
//! vector type is needed. Needs `pgvector` 0.8 or newer, for the iterative
//! index scans that let a search skip forgotten memories and still fill its
//! limit. The vector index is HNSW, which `pgvector` limits to 2000
//! dimensions.
//!
//! The queries are the `sqlx::query*!` macros, which check them against the
//! schema while compiling (see `AGENTS.md` for how). The ones that touch the
//! vector tables are plain `sqlx::query*` calls, because the database the
//! macros check against has no `pgvector`.

use std::str::FromStr;
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
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, FromRow, PgConnection, PgPool};

type Result<T> = std::result::Result<T, StoreError>;

/// A row of the `memories` table, as the database has it.
#[derive(FromRow)]
struct MemoryRow {
  id: i64,
  content: String,
  content_hash: String,
  event_count: i64,
  last_event_at: DateTime<Utc>,
  interest_score: Option<f32>,
  forgotten_at: Option<DateTime<Utc>>,
  version: i64,
  created_at: DateTime<Utc>,
  updated_at: DateTime<Utc>,
}

impl From<MemoryRow> for Memory {
  fn from(row: MemoryRow) -> Self {
    Self {
      id: MemoryId::new(row.id),
      content: row.content,
      content_hash: row.content_hash,
      event_count: row.event_count,
      last_event_at: row.last_event_at,
      interest_score: row.interest_score,
      forgotten_at: row.forgotten_at,
      version: Version::new(row.version),
      created_at: row.created_at,
      updated_at: row.updated_at,
    }
  }
}

/// A memory row and its distance from the vector that was searched for.
#[derive(FromRow)]
struct CandidateRow {
  #[sqlx(flatten)]
  memory: MemoryRow,
  distance: f32,
}

impl From<CandidateRow> for Candidate {
  fn from(row: CandidateRow) -> Self {
    Self {
      memory: row.memory.into(),
      distance: row.distance,
    }
  }
}

/// A memory row and its vector as text.
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
      memory: row.memory.into(),
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
  created_at: DateTime<Utc>,
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
      created_at: row.created_at,
    })
  }
}

/// A store on one Postgres database.
pub struct PostgresStore {
  pool: PgPool,
  fingerprint: Fingerprint,
}

impl PostgresStore {
  /// Vectors are stored with `fingerprint`, and its dimension sizes the vector
  /// tables.
  pub async fn connect(
    config: &DatabaseConfig,
    fingerprint: Fingerprint,
  ) -> Result<Self> {
    let options = PgConnectOptions::from_str(&config.url)
      .map_err(StoreError::backend("invalid Postgres connection URL"))?;
    let pool = PgPoolOptions::new()
      .max_connections(config.max_connections)
      .min_connections(config.min_connections)
      .acquire_timeout(config.acquire_timeout)
      .connect_with(options)
      .await
      .map_err(StoreError::backend("could not open the Postgres database"))?;
    Ok(Self { pool, fingerprint })
  }

  async fn write_new(
    &self,
    connection: &mut PgConnection,
    event: &NewEvent,
    memory: &MemoryText,
  ) -> Result<WriteOutcome> {
    lock_text(connection, &memory.hash).await?;
    if let Some(id) = holder_of(connection, &memory.hash).await? {
      return Ok(WriteOutcome::Duplicate(id));
    }

    let id = sqlx::query_scalar!(
      r#"INSERT INTO memories
           (content, content_hash, event_count, interest_score)
         VALUES ($1, $2, 1, $3)
         RETURNING id"#,
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
    connection: &mut PgConnection,
    memory_id: MemoryId,
    version: Version,
    event: &NewEvent,
    memory: &MemoryText,
  ) -> Result<WriteOutcome> {
    lock_text(connection, &memory.hash).await?;
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
         content = $2,
         content_hash = $3,
         interest_score = $4,
         event_count = event_count + 1,
         last_event_at = now(),
         forgotten_at = NULL,
         version = version + 1,
         updated_at = now()
       WHERE id = $1",
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
    connection: &mut PgConnection,
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
       SELECT id, $2, $3, content FROM memories WHERE id = $1",
      memory_id.get(),
      event.content,
      metadata,
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query!(
      "UPDATE memories SET
         event_count = event_count + 1,
         last_event_at = now(),
         forgotten_at = NULL,
         version = version + 1,
         updated_at = now()
       WHERE id = $1",
      memory_id.get(),
    )
    .execute(&mut *connection)
    .await?;
    Ok(WriteOutcome::Written(memory_id))
  }

  async fn put_vector(
    &self,
    connection: &mut PgConnection,
    memory_id: MemoryId,
    vector: &[f32],
  ) -> Result<()> {
    sqlx::query(
      "INSERT INTO memory_vectors (memory_id, embedding, fingerprint)
       VALUES ($1, $2::text::vector, $3)
       ON CONFLICT (memory_id) DO UPDATE SET
         embedding = EXCLUDED.embedding,
         fingerprint = EXCLUDED.fingerprint",
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
    connection: &mut PgConnection,
    table: &str,
    key: &str,
    parent: &str,
  ) -> Result<()> {
    let dimension = self.fingerprint.dimension;
    let create = format!(
      "CREATE TABLE IF NOT EXISTS {table} (
         {key} BIGINT PRIMARY KEY REFERENCES {parent} (id),
         embedding vector({dimension}) NOT NULL,
         fingerprint TEXT NOT NULL
       )"
    );
    let index = format!(
      "CREATE INDEX IF NOT EXISTS {table}_embedding
       ON {table} USING hnsw (embedding vector_cosine_ops)"
    );
    // The names are the ones the callers pass and the dimension is a number.
    for statement in [create, index] {
      sqlx::query(AssertSqlSafe(statement))
        .execute(&mut *connection)
        .await
        .map_err(StoreError::backend("could not create a vector table"))?;
    }
    Ok(())
  }
}

/// Waits for any other change to the vector tables, until the transaction
/// ends. Creating the same table twice at once fails, and so does swapping
/// while another instance creates.
async fn lock_vector_tables(connection: &mut PgConnection) -> Result<()> {
  sqlx::query!(
    "SELECT pg_advisory_xact_lock(
       hashtextextended('mcp-semantic-memory vector tables', 0)
     )"
  )
  .execute(&mut *connection)
  .await?;
  Ok(())
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

/// The words of a text as a `tsquery`: any of them may match. They are only
/// letters and digits, so nothing in them can be taken for query syntax.
fn match_query(text: &str) -> Option<String> {
  let words = keywords(text);
  (!words.is_empty()).then(|| words.join(" | "))
}

/// Waits for any other write of this text, until the transaction ends. Two
/// writers could otherwise both find the text free and both take it.
async fn lock_text(connection: &mut PgConnection, hash: &str) -> Result<()> {
  sqlx::query!(
    "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
    hash
  )
  .execute(&mut *connection)
  .await?;
  Ok(())
}

/// The memory that already has this text.
async fn holder_of(
  connection: &mut PgConnection,
  hash: &str,
) -> Result<Option<MemoryId>> {
  let id = sqlx::query_scalar!(
    "SELECT id FROM memories WHERE content_hash = $1",
    hash
  )
  .fetch_optional(&mut *connection)
  .await?;
  Ok(id.map(MemoryId::new))
}

/// The memory that already has this text, other than `memory_id`.
async fn other_holder_of(
  connection: &mut PgConnection,
  hash: &str,
  memory_id: MemoryId,
) -> Result<Option<MemoryId>> {
  let id = sqlx::query_scalar!(
    "SELECT id FROM memories WHERE content_hash = $1 AND id != $2",
    hash,
    memory_id.get(),
  )
  .fetch_optional(&mut *connection)
  .await?;
  Ok(id.map(MemoryId::new))
}

/// The version of the memory. The row stays locked until the transaction
/// ends, so what the caller checked it against cannot change under it.
async fn version_of(
  connection: &mut PgConnection,
  memory_id: MemoryId,
) -> Result<Version> {
  sqlx::query_scalar!(
    "SELECT version FROM memories WHERE id = $1 FOR UPDATE",
    memory_id.get()
  )
  .fetch_optional(&mut *connection)
  .await?
  .map(Version::new)
  .ok_or(StoreError::MissingMemory(memory_id))
}

async fn insert_event(
  connection: &mut PgConnection,
  memory_id: MemoryId,
  event: &NewEvent,
  memory_text: &str,
) -> Result<()> {
  let metadata = event.metadata.to_string();
  sqlx::query!(
    "INSERT INTO events (memory_id, content, metadata, memory_text)
     VALUES ($1, $2, $3, $4)",
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
impl Store for PostgresStore {
  async fn migrate(&self) -> Result<()> {
    // NOTE: sqlx takes an advisory lock of its own around the migrations
    sqlx::migrate!("./migrations")
      .run(&self.pool)
      .await
      .map_err(StoreError::backend("could not run the migrations"))?;

    // NOTE: instances may start together, and creating the same table twice
    // at once fails, so one of them goes first
    let mut tx = self.pool.begin().await?;
    lock_vector_tables(&mut tx).await?;
    sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
      .execute(&mut *tx)
      .await
      .map_err(StoreError::backend("could not create the vector extension"))?;
    for (table, key, parent) in [
      ("memory_vectors", "memory_id", "memories"),
      ("interest_vectors", "interest_id", "interests"),
    ] {
      self
        .create_vector_table(&mut tx, table, key, parent)
        .await?;
    }
    tx.commit().await?;
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

    let mut tx = self.pool.begin().await?;
    // NOTE: the index returns its nearest neighbours before the filter on
    // forgotten memories is applied, so it could come back with fewer than
    // `limit`. Iterative scans make it keep looking, in order.
    sqlx::query("SET LOCAL hnsw.iterative_scan = strict_order")
      .execute(&mut *tx)
      .await
      .map_err(StoreError::backend("could not set up the vector search"))?;
    let rows: Vec<CandidateRow> = sqlx::query_as(
      "SELECT m.*, (v.embedding <=> $1::text::vector)::real AS distance
       FROM memory_vectors v
       JOIN memories m ON m.id = v.memory_id
       WHERE $2 OR m.forgotten_at IS NULL
       ORDER BY v.embedding <=> $1::text::vector
       LIMIT $3",
    )
    .bind(vector_json(vector)?)
    .bind(forgotten == Forgotten::Include)
    .bind(limit_number(limit))
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::backend("could not search the vectors"))?;
    Ok(rows.into_iter().map(Candidate::from).collect())
  }

  async fn keyword(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
    let Some(query) = match_query(query) else {
      return Ok(Vec::new());
    };
    let limit = limit_number(limit);
    let rows = sqlx::query_as!(
      MemoryRow,
      "SELECT m.*
       FROM memories m
       WHERE m.forgotten_at IS NULL
         AND to_tsvector('simple', m.content) @@ to_tsquery('simple', $1)
       ORDER BY
         ts_rank(to_tsvector('simple', m.content), to_tsquery('simple', $1))
           DESC,
         m.id
       LIMIT $2",
      query,
      limit,
    )
    .fetch_all(&self.pool)
    .await
    .map_err(StoreError::backend("could not search the keywords"))?;
    Ok(rows.into_iter().map(Memory::from).collect())
  }

  async fn write(&self, write: Write) -> Result<WriteOutcome> {
    let mut tx = self.pool.begin().await?;

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
    Ok(
      sqlx::query_as!(
        MemoryRow,
        "SELECT * FROM memories WHERE id = $1",
        id.get()
      )
      .fetch_optional(&self.pool)
      .await?
      .map(Memory::from),
    )
  }

  async fn events(
    &self,
    memory_id: MemoryId,
    limit: usize,
  ) -> Result<Vec<Event>> {
    let limit = limit_number(limit);
    let rows = sqlx::query_as!(
      EventRow,
      "SELECT * FROM events WHERE memory_id = $1 ORDER BY id DESC LIMIT $2",
      memory_id.get(),
      limit,
    )
    .fetch_all(&self.pool)
    .await?;
    rows.into_iter().map(Event::try_from).collect()
  }

  async fn interests(&self) -> Result<Vec<Vector>> {
    let vectors: Vec<String> = sqlx::query_scalar(
      "SELECT v.embedding::text
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
      "SELECT m.*, v.embedding::text AS vector
       FROM memories m
       JOIN memory_vectors v ON v.memory_id = m.id
       WHERE m.forgotten_at IS NULL
       ORDER BY m.event_count DESC, m.last_event_at DESC, m.id
       LIMIT $1",
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
      "SELECT m.*, v.embedding::text AS vector
       FROM memories m
       JOIN memory_vectors v ON v.memory_id = m.id
       WHERE m.forgotten_at IS NULL AND m.id > $1
       ORDER BY m.id
       LIMIT $2",
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
    // NOTE: an advisory lock only one instance can hold, taken before
    // anything is read. Statements read what the one before it committed, so
    // after waiting for the lock the check sees the run that came first.
    let mut tx = self.pool.begin().await?;
    sqlx::query!(
      "SELECT pg_advisory_xact_lock(
         hashtextextended('mcp-semantic-memory consolidation', 0)
       )"
    )
    .execute(&mut *tx)
    .await?;

    if let Some(max_age) = only_if_needed {
      let health = health_of(&mut tx, &self.fingerprint).await?;
      if !health.needs_consolidation(max_age) {
        return Ok(Consolidated::NotStale);
      }
    }

    // NOTE: the clock and not the start of the transaction, which may be from
    // before the wait for the lock, and at least a microsecond after the run
    // before, or two runs at the same time would count as one
    let run_at = sqlx::query_scalar!(
      r#"SELECT GREATEST(
           clock_timestamp(),
           COALESCE(
             (SELECT max(run_at) FROM interests) + interval '1 microsecond',
             '-infinity'::timestamptz
           )
         ) AS "run_at!""#
    )
    .fetch_one(&mut *tx)
    .await?;
    for interest in &consolidation.interests {
      let id = sqlx::query_scalar!(
        "INSERT INTO interests (run_at, top_terms)
         VALUES ($1, $2)
         RETURNING id",
        run_at,
        interest.top_terms,
      )
      .fetch_one(&mut *tx)
      .await?;
      sqlx::query(
        "INSERT INTO interest_vectors (interest_id, embedding, fingerprint)
         VALUES ($1, $2::text::vector, $3)",
      )
      .bind(id)
      .bind(vector_json(&interest.centre)?)
      .bind(self.fingerprint.to_string())
      .execute(&mut *tx)
      .await
      .map_err(StoreError::backend("could not store an interest"))?;
    }

    for verdict in &consolidation.verdicts {
      sqlx::query!(
        "UPDATE memories SET
           interest_score = $3,
           forgotten_at = CASE WHEN $4 THEN $5 ELSE forgotten_at END
         WHERE id = $1 AND version = $2",
        verdict.memory_id.get(),
        verdict.version.get(),
        verdict.interest_score,
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
    let mut tx = self.pool.begin().await?;
    lock_vector_tables(&mut tx).await?;
    let next = "memory_vectors_next";
    self
      .create_vector_table(&mut tx, next, "memory_id", "memories")
      .await?;
    let kept: i64 = sqlx::query_scalar(
      "SELECT count(*) FROM memory_vectors_next WHERE fingerprint = $1",
    )
    .bind(self.fingerprint.to_string())
    .fetch_one(&mut *tx)
    .await?;
    if kept == 0 {
      sqlx::query("DROP TABLE memory_vectors_next")
        .execute(&mut *tx)
        .await?;
      self
        .create_vector_table(&mut tx, next, "memory_id", "memories")
        .await?;
    }
    tx.commit().await?;
    Ok(())
  }

  async fn unembedded(&self, limit: usize) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = sqlx::query_as(
      "SELECT m.*
       FROM memories m
       LEFT JOIN memory_vectors_next v ON v.memory_id = m.id
       WHERE v.memory_id IS NULL OR v.fingerprint != $1
       ORDER BY m.id
       LIMIT $2",
    )
    .bind(self.fingerprint.to_string())
    .bind(limit_number(limit))
    .fetch_all(&self.pool)
    .await
    .map_err(StoreError::backend("could not read the memories to embed"))?;
    Ok(rows.into_iter().map(Memory::from).collect())
  }

  async fn put_reembedded(
    &self,
    vectors: Vec<(MemoryId, Vector)>,
  ) -> Result<()> {
    let mut tx = self.pool.begin().await?;
    for (memory_id, vector) in vectors {
      sqlx::query(
        "INSERT INTO memory_vectors_next (memory_id, embedding, fingerprint)
         VALUES ($1, $2::text::vector, $3)
         ON CONFLICT (memory_id) DO UPDATE SET
           embedding = EXCLUDED.embedding,
           fingerprint = EXCLUDED.fingerprint",
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
    let mut tx = self.pool.begin().await?;
    lock_vector_tables(&mut tx).await?;

    // NOTE: new memories wait on the table lock that the swap takes, so
    // none can slip in between this check and the swap
    sqlx::query("LOCK TABLE memories IN SHARE MODE")
      .execute(&mut *tx)
      .await?;
    let missing: i64 = sqlx::query_scalar(
      "SELECT count(*)
       FROM memories m
       LEFT JOIN memory_vectors_next v ON v.memory_id = m.id
       WHERE v.memory_id IS NULL OR v.fingerprint != $1",
    )
    .bind(self.fingerprint.to_string())
    .fetch_one(&mut *tx)
    .await?;
    if missing > 0 {
      return Ok(Swapped::Missing);
    }

    // NOTE: the index and the key of the new table are named after it, and
    // the next reembed makes a table of that name again
    for statement in [
      "DROP TABLE memory_vectors",
      "ALTER TABLE memory_vectors_next RENAME TO memory_vectors",
      "ALTER INDEX memory_vectors_next_embedding
       RENAME TO memory_vectors_embedding",
      "ALTER TABLE memory_vectors
       RENAME CONSTRAINT memory_vectors_next_pkey TO memory_vectors_pkey",
      "DROP TABLE interest_vectors",
    ] {
      sqlx::query(statement)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::backend("could not swap the vector tables"))?;
    }
    self
      .create_vector_table(
        &mut tx,
        "interest_vectors",
        "interest_id",
        "interests",
      )
      .await?;
    tx.commit().await?;
    Ok(Swapped::Done)
  }
}

/// What the startup checks need, read on one connection so a transaction can
/// ask too.
async fn health_of(
  connection: &mut PgConnection,
  fingerprint: &Fingerprint,
) -> Result<Health> {
  let fingerprint = fingerprint.to_string();

  let memories: i64 = sqlx::query_scalar(
    "SELECT count(*)
     FROM memories m
     LEFT JOIN memory_vectors v ON v.memory_id = m.id
     WHERE m.forgotten_at IS NULL
       AND (v.fingerprint IS NULL OR v.fingerprint != $1)",
  )
  .bind(&fingerprint)
  .fetch_one(&mut *connection)
  .await?;

  let interests: i64 = sqlx::query_scalar(
    "SELECT count(*)
     FROM interests i
     LEFT JOIN interest_vectors v ON v.interest_id = i.id
     WHERE i.run_at = (SELECT max(run_at) FROM interests)
       AND (v.fingerprint IS NULL OR v.fingerprint != $1)",
  )
  .bind(&fingerprint)
  .fetch_one(&mut *connection)
  .await?;

  // NOTE: the times are the database's, and the newest event stands in for
  // the time now, so a quiet spell is not a reason to consolidate
  let seconds = sqlx::query_scalar!(
    r#"SELECT EXTRACT(EPOCH FROM (
         (SELECT max(last_event_at) FROM memories WHERE forgotten_at IS NULL)
         - COALESCE(
           (SELECT max(run_at) FROM interests),
           (SELECT min(created_at) FROM memories WHERE forgotten_at IS NULL)
         )
       ))::float8 AS "seconds?""#
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
  use super::*;

  #[test]
  fn the_query_words_are_joined_with_or() {
    assert_eq!(match_query("Cat, cat! sat?").as_deref(), Some("cat | sat"));
    assert_eq!(match_query("  ... \" ' ( ) "), None);
  }
}
