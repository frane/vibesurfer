-- Intercepting sessions route their pages through a daemon-side HTTP
-- proxy. The flag survives a restart so rebuilt pages are proxied
-- again (by a fresh proxy; held requests and events do not survive).
ALTER TABLE sessions ADD COLUMN intercept INTEGER NOT NULL DEFAULT 0;
