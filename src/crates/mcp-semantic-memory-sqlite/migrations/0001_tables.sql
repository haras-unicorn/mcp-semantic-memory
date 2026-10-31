-- Times are text in the RFC 3339 form, and they come from the database clock
-- and not from the instances.

CREATE TABLE memories (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  content TEXT NOT NULL,
  content_hash TEXT NOT NULL UNIQUE,
  event_count INTEGER NOT NULL,
  last_event_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  interest_score REAL,
  forgotten_at TEXT,
  version INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX memories_forgotten_at ON memories (forgotten_at);

-- Never changed after it is written, and never deleted.
CREATE TABLE events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  memory_id INTEGER NOT NULL REFERENCES memories (id),
  content TEXT NOT NULL,
  metadata TEXT NOT NULL,
  memory_text TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX events_memory_id ON events (memory_id, id);

-- One row per interest per consolidation run. The rows of the latest run are
-- the current interests.
CREATE TABLE interests (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  run_at TEXT NOT NULL,
  top_terms TEXT NOT NULL
);

CREATE INDEX interests_run_at ON interests (run_at);
