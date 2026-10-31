-- Keyword search over the memory text.

ALTER TABLE memories ADD FULLTEXT INDEX memories_content_fts (content);
