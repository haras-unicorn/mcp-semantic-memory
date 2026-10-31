-- Times come from the database clock and not from the instances, and the
-- connection keeps it in UTC.

CREATE TABLE memories (
  id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
  content TEXT NOT NULL,
  content_hash VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
  event_count BIGINT NOT NULL,
  last_event_at TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
  interest_score FLOAT NULL,
  forgotten_at TIMESTAMP(6) NULL DEFAULT NULL,
  version BIGINT NOT NULL DEFAULT 1,
  created_at TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
  updated_at TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
  UNIQUE KEY memories_content_hash (content_hash),
  KEY memories_forgotten_at (forgotten_at)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

-- Never changed after it is written, and never deleted.
CREATE TABLE events (
  id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
  memory_id BIGINT NOT NULL,
  content TEXT NOT NULL,
  metadata LONGTEXT NOT NULL,
  memory_text TEXT NOT NULL,
  created_at TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
  KEY events_memory_id (memory_id, id),
  CONSTRAINT events_memory FOREIGN KEY (memory_id) REFERENCES memories (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

-- One row per interest per consolidation run. The rows of the latest run are
-- the current interests.
CREATE TABLE interests (
  id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
  run_at TIMESTAMP(6) NOT NULL,
  top_terms TEXT NOT NULL,
  KEY interests_run_at (run_at)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;
