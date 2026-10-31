//! The MariaDB store: sqlx with plain SQL, MariaDB's own `VECTOR` type and
//! index for the vectors and an InnoDB `FULLTEXT` index for the keywords.
//!
//! Vectors go in and out as text (`VEC_FromText` and `VEC_ToText`), so no
//! driver-specific vector type is needed. Needs MariaDB 11.7 or newer. Words
//! shorter than `innodb_ft_min_token_size` and the InnoDB stopwords are never
//! found by a keyword search.
//!
//! The queries are the `sqlx::query*!` macros, which check them against the
//! schema while compiling (see `AGENTS.md` for how). The ones that touch the
//! vector tables are plain `sqlx::query*` calls, because the database the
//! macros check against has no vector tables.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use mcp_semantic_memory::config::DatabaseConfig;
use mcp_semantic_memory::store::{
  Consolidated, Consolidation, Embedded, Forgotten, Health, MemoryText, Store,
  StoreError, Swapped, Write, WriteOutcome, keywords,
};
use mcp_semantic_memory::types::{
  Candidate, Event, EventId, Fingerprint, Memory, MemoryId, NewEvent, Vector,
  Version,
};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use sqlx::{AssertSqlSafe, Connection, FromRow, MySqlConnection, MySqlPool};

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
  embedding: String,
}

impl TryFrom<EmbeddedRow> for Embedded {
  type Error = StoreError;

  fn try_from(row: EmbeddedRow) -> Result<Self> {
    Ok(Self {
      memory: row.memory.into(),
      vector: parse_vector(&row.embedding)?,
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

/// A store on one MariaDB database.
pub struct MariaDbStore {
  pool: MySqlPool,
  fingerprint: Fingerprint,
}

impl MariaDbStore {
  /// Vectors are stored with `fingerprint`, and its dimension sizes the vector
  /// tables.
  pub async fn connect(
    config: &DatabaseConfig,
    fingerprint: Fingerprint,
  ) -> Result<Self> {
    let options = MySqlConnectOptions::from_str(&config.url)
      .map_err(StoreError::backend("invalid MariaDB connection URL"))?;
    let pool = MySqlPoolOptions::new()
      .max_connections(config.max_connections)
      .min_connections(config.min_connections)
      .acquire_timeout(config.acquire_timeout)
      // NOTE: READ COMMITTED lets a writer see a text another writer has just
      // taken, which the default would hide behind its snapshot. Times are
      // UTC so they read back as UTC.
      .after_connect(|connection, _| {
        Box::pin(async move {
          sqlx::query("SET time_zone = '+00:00'")
            .execute(&mut *connection)
            .await?;
          sqlx::query("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *connection)
            .await?;
          Ok(())
        })
      })
      .connect_with(options)
      .await
      .map_err(StoreError::backend("could not open the MariaDB database"))?;
    Ok(Self { pool, fingerprint })
  }

  async fn write_new(
    &self,
    connection: &mut MySqlConnection,
    event: &NewEvent,
    memory: &MemoryText,
  ) -> Result<WriteOutcome> {
    if let Some(id) = holder_of(connection, &memory.hash).await? {
      return Ok(WriteOutcome::Duplicate(id));
    }

    let inserted = sqlx::query!(
      "INSERT INTO memories
         (content, content_hash, event_count, interest_score)
       VALUES (?, ?, 1, ?)",
      memory.text,
      memory.hash,
      memory.interest_score,
    )
    .execute(&mut *connection)
    .await;
    let id = match inserted {
      Ok(result) => {
        let id = i64::try_from(result.last_insert_id()).map_err(|_| {
          StoreError::Corrupt("a memory id does not fit a number".to_owned())
        })?;
        MemoryId::new(id)
      }
      Err(error) if is_duplicate(&error) => {
        return match holder_of(connection, &memory.hash).await? {
          Some(id) => Ok(WriteOutcome::Duplicate(id)),
          None => Err(error.into()),
        };
      }
      Err(error) => return Err(error.into()),
    };
    insert_event(connection, id, event, &memory.text).await?;
    self.put_vector(connection, id, &memory.vector).await?;
    Ok(WriteOutcome::Written(id))
  }

  async fn write_attach(
    &self,
    connection: &mut MySqlConnection,
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

    let updated = sqlx::query!(
      "UPDATE memories SET
         content = ?,
         content_hash = ?,
         interest_score = ?,
         event_count = event_count + 1,
         last_event_at = CURRENT_TIMESTAMP(6),
         forgotten_at = NULL,
         version = version + 1,
         updated_at = CURRENT_TIMESTAMP(6)
       WHERE id = ?",
      memory.text,
      memory.hash,
      memory.interest_score,
      memory_id.get(),
    )
    .execute(&mut *connection)
    .await;
    match updated {
      Ok(_) => {}
      Err(error) if is_duplicate(&error) => {
        return match other_holder_of(connection, &memory.hash, memory_id)
          .await?
        {
          Some(id) => Ok(WriteOutcome::Duplicate(id)),
          None => Err(error.into()),
        };
      }
      Err(error) => return Err(error.into()),
    }
    insert_event(connection, memory_id, event, &memory.text).await?;
    self
      .put_vector(connection, memory_id, &memory.vector)
      .await?;
    Ok(WriteOutcome::Written(memory_id))
  }

  async fn write_repeat(
    &self,
    connection: &mut MySqlConnection,
    memory_id: MemoryId,
    version: Version,
    event: &NewEvent,
  ) -> Result<WriteOutcome> {
    if version_of(connection, memory_id).await? != version {
      return Ok(WriteOutcome::Conflict);
    }

    let text = sqlx::query_scalar!(
      "SELECT content FROM memories WHERE id = ?",
      memory_id.get()
    )
    .fetch_one(&mut *connection)
    .await?;
    insert_event(connection, memory_id, event, &text).await?;
    sqlx::query!(
      "UPDATE memories SET
         event_count = event_count + 1,
         last_event_at = CURRENT_TIMESTAMP(6),
         forgotten_at = NULL,
         version = version + 1,
         updated_at = CURRENT_TIMESTAMP(6)
       WHERE id = ?",
      memory_id.get(),
    )
    .execute(&mut *connection)
    .await?;
    Ok(WriteOutcome::Written(memory_id))
  }

  async fn put_vector(
    &self,
    connection: &mut MySqlConnection,
    memory_id: MemoryId,
    vector: &[f32],
  ) -> Result<()> {
    sqlx::query(
      "INSERT INTO memory_vectors (memory_id, embedding, fingerprint)
       VALUES (?, VEC_FromText(?), ?)
       ON DUPLICATE KEY UPDATE
         embedding = VALUES(embedding),
         fingerprint = VALUES(fingerprint)",
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
    connection: &mut MySqlConnection,
    table: &str,
    key: &str,
  ) -> Result<()> {
    let dimension = self.fingerprint.dimension;
    let create = format!(
      "CREATE TABLE IF NOT EXISTS {table} (
         {key} BIGINT NOT NULL PRIMARY KEY,
         embedding VECTOR({dimension}) NOT NULL,
         fingerprint VARCHAR(255) NOT NULL,
         VECTOR INDEX {table}_embedding (embedding) DISTANCE = cosine
       ) ENGINE = InnoDB"
    );
    // The names are the ones the callers pass and the dimension is a number.
    sqlx::query(AssertSqlSafe(create))
      .execute(&mut *connection)
      .await
      .map_err(StoreError::backend("could not create a vector table"))?;
    Ok(())
  }

  /// Makes the table the reembed fills, or keeps the one an earlier run left
  /// when it holds vectors of this store's model.
  async fn prepare_next(&self, connection: &mut MySqlConnection) -> Result<()> {
    self
      .create_vector_table(connection, "memory_vectors_next", "memory_id")
      .await?;
    let kept: i64 = sqlx::query_scalar(
      "SELECT count(*) FROM memory_vectors_next WHERE fingerprint = ?",
    )
    .bind(self.fingerprint.to_string())
    .fetch_one(&mut *connection)
    .await?;
    if kept == 0 {
      sqlx::query("DROP TABLE memory_vectors_next")
        .execute(&mut *connection)
        .await?;
      self
        .create_vector_table(connection, "memory_vectors_next", "memory_id")
        .await?;
    }
    Ok(())
  }

  /// The swap itself, once the lock is held and nothing is missing. The
  /// interest vectors go first: if this stops half way, the memories still
  /// have their old vectors and the next reembed picks up and swaps.
  async fn swap_tables(&self, connection: &mut MySqlConnection) -> Result<()> {
    sqlx::query("DROP TABLE IF EXISTS memory_vectors_old")
      .execute(&mut *connection)
      .await?;
    sqlx::query("DROP TABLE interest_vectors")
      .execute(&mut *connection)
      .await?;
    self
      .create_vector_table(connection, "interest_vectors", "interest_id")
      .await?;
    // NOTE: MariaDB cannot swap tables in a transaction, but one rename
    // statement of both is atomic
    sqlx::query(
      "RENAME TABLE memory_vectors TO memory_vectors_old,
                    memory_vectors_next TO memory_vectors",
    )
    .execute(&mut *connection)
    .await
    .map_err(StoreError::backend("could not swap the vector tables"))?;
    sqlx::query("DROP TABLE memory_vectors_old")
      .execute(&mut *connection)
      .await?;
    Ok(())
  }
}

/// Takes the named lock that guards changes to the vector tables, waiting as
/// long as the server lets a lock wait. It belongs to the connection, so
/// `unlock_vector_tables` has to give it back on that connection whether or
/// not the change worked.
async fn lock_vector_tables(connection: &mut MySqlConnection) -> Result<()> {
  let locked = sqlx::query_scalar!(
    "SELECT GET_LOCK('mcp-semantic-memory vector tables',
                     @@lock_wait_timeout) AS locked"
  )
  .fetch_one(&mut *connection)
  .await?;
  if locked != Some(1) {
    return Err(StoreError::Corrupt(
      "could not take the lock for the vector tables".to_owned(),
    ));
  }
  Ok(())
}

async fn unlock_vector_tables(connection: &mut MySqlConnection) -> Result<()> {
  sqlx::query!(
    "SELECT RELEASE_LOCK('mcp-semantic-memory vector tables') AS released"
  )
  .fetch_one(&mut *connection)
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

/// The words of a text, for a natural language search: any of them may match.
/// They are only letters and digits, so nothing in them can be taken for
/// query syntax.
fn match_query(text: &str) -> Option<String> {
  let words = keywords(text);
  (!words.is_empty()).then(|| words.join(" "))
}

/// Whether a statement failed because a text is already taken.
fn is_duplicate(error: &sqlx::Error) -> bool {
  matches!(error, sqlx::Error::Database(error) if error.is_unique_violation())
}

/// The memory that already has this text.
async fn holder_of(
  connection: &mut MySqlConnection,
  hash: &str,
) -> Result<Option<MemoryId>> {
  let id =
    sqlx::query_scalar!("SELECT id FROM memories WHERE content_hash = ?", hash)
      .fetch_optional(&mut *connection)
      .await?;
  Ok(id.map(MemoryId::new))
}

/// The memory that already has this text, other than `memory_id`.
async fn other_holder_of(
  connection: &mut MySqlConnection,
  hash: &str,
  memory_id: MemoryId,
) -> Result<Option<MemoryId>> {
  let id = sqlx::query_scalar!(
    "SELECT id FROM memories WHERE content_hash = ? AND id != ?",
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
  connection: &mut MySqlConnection,
  memory_id: MemoryId,
) -> Result<Version> {
  sqlx::query_scalar!(
    "SELECT version FROM memories WHERE id = ? FOR UPDATE",
    memory_id.get()
  )
  .fetch_optional(&mut *connection)
  .await?
  .map(Version::new)
  .ok_or(StoreError::MissingMemory(memory_id))
}

async fn insert_event(
  connection: &mut MySqlConnection,
  memory_id: MemoryId,
  event: &NewEvent,
  memory_text: &str,
) -> Result<()> {
  let metadata = event.metadata.to_string();
  sqlx::query!(
    "INSERT INTO events (memory_id, content, metadata, memory_text)
     VALUES (?, ?, ?, ?)",
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
impl Store for MariaDbStore {
  async fn migrate(&self) -> Result<()> {
    // NOTE: sqlx takes a named lock of its own around the migrations
    sqlx::migrate!("./migrations")
      .run(&self.pool)
      .await
      .map_err(StoreError::backend("could not run the migrations"))?;

    // NOTE: instances may start together, so one of them goes first, and the
    // others wait
    let mut connection = self.pool.acquire().await?;
    lock_vector_tables(&mut connection).await?;
    let mut created = Ok(());
    for (table, key) in [
      ("memory_vectors", "memory_id"),
      ("interest_vectors", "interest_id"),
    ] {
      created = self.create_vector_table(&mut connection, table, key).await;
      if created.is_err() {
        break;
      }
    }
    unlock_vector_tables(&mut connection).await?;
    created
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

    // NOTE: the vector index only works inside the inner query and cannot skip
    // forgotten memories, so ask it for enough neighbours that `limit` of
    // them are left even if every forgotten memory is nearer than all the
    // others
    let wanted = match forgotten {
      Forgotten::Include => limit,
      Forgotten::Skip => {
        let forgotten = sqlx::query_scalar!(
          "SELECT count(*) FROM memories WHERE forgotten_at IS NOT NULL"
        )
        .fetch_one(&self.pool)
        .await?;
        limit.saturating_add(usize::try_from(forgotten).unwrap_or(0))
      }
    };

    let vector = vector_json(vector)?;
    let rows: Vec<CandidateRow> = sqlx::query_as(
      "SELECT m.*, v.distance
       FROM (
         SELECT memory_id,
                CAST(VEC_DISTANCE_COSINE(embedding, VEC_FromText(?))
                     AS FLOAT) AS distance
         FROM memory_vectors
         ORDER BY VEC_DISTANCE_COSINE(embedding, VEC_FromText(?))
         LIMIT ?
       ) v
       JOIN memories m ON m.id = v.memory_id
       WHERE ? OR m.forgotten_at IS NULL
       ORDER BY v.distance
       LIMIT ?",
    )
    .bind(&vector)
    .bind(&vector)
    .bind(limit_number(wanted))
    .bind(forgotten == Forgotten::Include)
    .bind(limit_number(limit))
    .fetch_all(&self.pool)
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
         AND MATCH (m.content) AGAINST (? IN NATURAL LANGUAGE MODE)
       ORDER BY
         MATCH (m.content) AGAINST (? IN NATURAL LANGUAGE MODE) DESC,
         m.id
       LIMIT ?",
      query,
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
        "SELECT * FROM memories WHERE id = ?",
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
      "SELECT * FROM events WHERE memory_id = ? ORDER BY id DESC LIMIT ?",
      memory_id.get(),
      limit,
    )
    .fetch_all(&self.pool)
    .await?;
    rows.into_iter().map(Event::try_from).collect()
  }

  async fn interests(&self) -> Result<Vec<Vector>> {
    let vectors: Vec<String> = sqlx::query_scalar(
      "SELECT VEC_ToText(v.embedding)
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
      "SELECT m.*, VEC_ToText(v.embedding) AS embedding
       FROM memories m
       JOIN memory_vectors v ON v.memory_id = m.id
       WHERE m.forgotten_at IS NULL
       ORDER BY m.event_count DESC, m.last_event_at DESC, m.id
       LIMIT ?",
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
      "SELECT m.*, VEC_ToText(v.embedding) AS embedding
       FROM memories m
       JOIN memory_vectors v ON v.memory_id = m.id
       WHERE m.forgotten_at IS NULL AND m.id > ?
       ORDER BY m.id
       LIMIT ?",
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
    // NOTE: a named lock only one instance can hold, taken before anything is
    // read. It belongs to the connection, so it is taken and given back on
    // the one connection, whether or not the write worked.
    let mut connection = self.pool.acquire().await?;
    let locked = sqlx::query_scalar!(
      "SELECT GET_LOCK('mcp-semantic-memory consolidation',
                       @@lock_wait_timeout) AS locked"
    )
    .fetch_one(&mut *connection)
    .await?;
    if locked != Some(1) {
      return Err(StoreError::Corrupt(
        "could not take the lock for the consolidation".to_owned(),
      ));
    }
    let written = self
      .write_consolidation(&mut connection, &consolidation, only_if_needed)
      .await;
    sqlx::query!(
      "SELECT RELEASE_LOCK('mcp-semantic-memory consolidation') AS released"
    )
    .fetch_one(&mut *connection)
    .await?;
    written
  }

  async fn begin_reembed(&self) -> Result<()> {
    let mut connection = self.pool.acquire().await?;
    lock_vector_tables(&mut connection).await?;
    let prepared = self.prepare_next(&mut connection).await;
    unlock_vector_tables(&mut connection).await?;
    prepared
  }

  async fn unembedded(&self, limit: usize) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = sqlx::query_as(
      "SELECT m.*
       FROM memories m
       LEFT JOIN memory_vectors_next v ON v.memory_id = m.id
       WHERE v.memory_id IS NULL OR v.fingerprint != ?
       ORDER BY m.id
       LIMIT ?",
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
         VALUES (?, VEC_FromText(?), ?)
         ON DUPLICATE KEY UPDATE
           embedding = VALUES(embedding),
           fingerprint = VALUES(fingerprint)",
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
    let mut connection = self.pool.acquire().await?;
    lock_vector_tables(&mut connection).await?;
    let missing: Result<i64> = sqlx::query_scalar(
      "SELECT count(*)
       FROM memories m
       LEFT JOIN memory_vectors_next v ON v.memory_id = m.id
       WHERE v.memory_id IS NULL OR v.fingerprint != ?",
    )
    .bind(self.fingerprint.to_string())
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::from);
    let swapped = match missing {
      Ok(0) => self
        .swap_tables(&mut connection)
        .await
        .map(|()| Swapped::Done),
      Ok(_) => Ok(Swapped::Missing),
      Err(error) => Err(error),
    };
    unlock_vector_tables(&mut connection).await?;
    swapped
  }
}

impl MariaDbStore {
  /// The consolidation itself, once the lock is held. The session reads
  /// what the statement before it committed, so the check sees the run that
  /// came first.
  async fn write_consolidation(
    &self,
    connection: &mut MySqlConnection,
    consolidation: &Consolidation,
    only_if_needed: Option<Duration>,
  ) -> Result<Consolidated> {
    let mut tx = connection.begin().await?;

    if let Some(max_age) = only_if_needed {
      let health = health_of(&mut tx, &self.fingerprint).await?;
      if !health.needs_consolidation(max_age) {
        return Ok(Consolidated::NotStale);
      }
    }

    // NOTE: at least a microsecond after the run before, or two runs at the
    // same time would count as one. The session is in UTC.
    let run_at: NaiveDateTime = sqlx::query_scalar(
      "SELECT GREATEST(
         CURRENT_TIMESTAMP(6),
         COALESCE(
           DATE_ADD((SELECT max(run_at) FROM interests), INTERVAL 1 MICROSECOND),
           CURRENT_TIMESTAMP(6)
         )
       )",
    )
    .fetch_one(&mut *tx)
    .await?;
    let run_at = run_at.and_utc();
    for interest in &consolidation.interests {
      let inserted = sqlx::query!(
        "INSERT INTO interests (run_at, top_terms) VALUES (?, ?)",
        run_at,
        interest.top_terms,
      )
      .execute(&mut *tx)
      .await?;
      sqlx::query(
        "INSERT INTO interest_vectors (interest_id, embedding, fingerprint)
         VALUES (?, VEC_FromText(?), ?)",
      )
      .bind(inserted.last_insert_id())
      .bind(vector_json(&interest.centre)?)
      .bind(self.fingerprint.to_string())
      .execute(&mut *tx)
      .await
      .map_err(StoreError::backend("could not store an interest"))?;
    }

    for verdict in &consolidation.verdicts {
      sqlx::query!(
        "UPDATE memories SET
           interest_score = ?,
           forgotten_at = CASE WHEN ? THEN ? ELSE forgotten_at END
         WHERE id = ? AND version = ?",
        verdict.interest_score,
        verdict.forget,
        run_at,
        verdict.memory_id.get(),
        verdict.version.get(),
      )
      .execute(&mut *tx)
      .await?;
    }

    tx.commit().await?;
    Ok(Consolidated::Written)
  }
}

/// What the startup checks need, read on one connection so a transaction can
/// ask too.
async fn health_of(
  connection: &mut MySqlConnection,
  fingerprint: &Fingerprint,
) -> Result<Health> {
  let fingerprint = fingerprint.to_string();

  let memories: i64 = sqlx::query_scalar(
    "SELECT count(*)
     FROM memories m
     LEFT JOIN memory_vectors v ON v.memory_id = m.id
     WHERE m.forgotten_at IS NULL
       AND (v.fingerprint IS NULL OR v.fingerprint != ?)",
  )
  .bind(&fingerprint)
  .fetch_one(&mut *connection)
  .await?;

  let interests: i64 = sqlx::query_scalar(
    "SELECT count(*)
     FROM interests i
     LEFT JOIN interest_vectors v ON v.interest_id = i.id
     WHERE i.run_at = (SELECT max(run_at) FROM interests)
       AND (v.fingerprint IS NULL OR v.fingerprint != ?)",
  )
  .bind(&fingerprint)
  .fetch_one(&mut *connection)
  .await?;

  // NOTE: the times are the database's, and the newest event stands in for
  // the time now, so a quiet spell is not a reason to consolidate. A newest
  // event before the latest run is a negative number, which `count` makes
  // nothing.
  let micros = sqlx::query_scalar!(
    "SELECT TIMESTAMPDIFF(MICROSECOND, COALESCE(
       (SELECT max(run_at) FROM interests),
       (SELECT min(created_at) FROM memories WHERE forgotten_at IS NULL)
     ), (SELECT max(last_event_at) FROM memories WHERE forgotten_at IS NULL))"
  )
  .fetch_one(&mut *connection)
  .await?;
  let since_consolidation =
    micros.map(|micros| Duration::from_micros(count(micros)));

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
  fn the_query_words_are_joined_with_spaces() {
    assert_eq!(match_query("Cat, cat! sat?").as_deref(), Some("cat sat"));
    assert_eq!(match_query("  ... \" ' ( ) "), None);
  }
}
