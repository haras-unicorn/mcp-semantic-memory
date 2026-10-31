-- Keyword search over the memory text. The index reads its text from the
-- memories table, and the triggers keep it in step.

CREATE VIRTUAL TABLE memories_fts USING fts5 (
  content,
  content = 'memories',
  content_rowid = 'id',
  tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER memories_fts_insert AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts (rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER memories_fts_delete AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts (memories_fts, rowid, content)
  VALUES ('delete', old.id, old.content);
END;

CREATE TRIGGER memories_fts_update AFTER UPDATE OF content ON memories BEGIN
  INSERT INTO memories_fts (memories_fts, rowid, content)
  VALUES ('delete', old.id, old.content);
  INSERT INTO memories_fts (rowid, content) VALUES (new.id, new.content);
END;
