-- Isolated sessions use a private, in-memory browser data store that
-- the engine discards on close or daemon restart. The flag survives a
-- restart so rebuilt pages stay isolated; their storage does not.
ALTER TABLE sessions ADD COLUMN isolated INTEGER NOT NULL DEFAULT 0;
