-- Times come from the database clock and not from the instances.

CREATE TABLE memories (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  content TEXT NOT NULL,
  content_hash TEXT NOT NULL UNIQUE,
  event_count BIGINT NOT NULL,
  last_event_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  interest_score REAL,
  forgotten_at TIMESTAMPTZ,
  version BIGINT NOT NULL DEFAULT 1,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX memories_forgotten_at ON memories (forgotten_at);

-- Never changed after it is written, and never deleted.
CREATE TABLE events (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  memory_id BIGINT NOT NULL REFERENCES memories (id),
  content TEXT NOT NULL,
  metadata TEXT NOT NULL,
  memory_text TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX events_memory_id ON events (memory_id, id);

-- One row per interest per consolidation run. The rows of the latest run are
-- the current interests.
CREATE TABLE interests (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  run_at TIMESTAMPTZ NOT NULL,
  top_terms TEXT NOT NULL
);

CREATE INDEX interests_run_at ON interests (run_at);
