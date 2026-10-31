-- Keyword search over the memory text. The `simple` configuration only
-- lowercases and splits words, so no language is assumed.

CREATE INDEX memories_content_fts
  ON memories USING GIN (to_tsvector('simple', content));
