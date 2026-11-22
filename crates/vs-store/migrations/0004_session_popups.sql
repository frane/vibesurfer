-- Sessions whose pages open real popup windows (a new page per
-- target=_blank link or window.open) instead of loading them in place.
ALTER TABLE sessions ADD COLUMN popups INTEGER NOT NULL DEFAULT 0;
