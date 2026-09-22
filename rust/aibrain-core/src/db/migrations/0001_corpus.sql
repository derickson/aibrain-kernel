-- The corpus: one row per note, the links between them, and the layout cache.
--
-- Everything here is derived from the vaults and can be rebuilt by deleting the
-- database and rescanning. Nothing in Postgres is a source of truth about your
-- notes; the markdown files are.

CREATE TABLE IF NOT EXISTS brain (
    id          TEXT PRIMARY KEY,
    name        TEXT        NOT NULL,
    root        TEXT        NOT NULL,
    seed        INTEGER     NOT NULL DEFAULT 7,
    enabled     BOOLEAN     NOT NULL DEFAULT TRUE,
    note_count  INTEGER     NOT NULL DEFAULT 0,
    -- Bumped whenever anything in this brain changes, so a client holding a
    -- cached layout can ask "is mine still current?" with one integer.
    revision    BIGINT      NOT NULL DEFAULT 0,
    scanned_at  TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS note (
    id           BIGSERIAL PRIMARY KEY,
    brain_id     TEXT        NOT NULL REFERENCES brain(id) ON DELETE CASCADE,
    rel_path     TEXT        NOT NULL,
    title        TEXT        NOT NULL,
    source       TEXT        NOT NULL,
    excerpt      TEXT        NOT NULL DEFAULT '',
    body         TEXT        NOT NULL DEFAULT '',
    tags         TEXT[]      NOT NULL DEFAULT '{}',
    -- The same tags as one string. A generated tsvector column must be built
    -- from IMMUTABLE expressions only, and array_to_string is merely STABLE,
    -- so the flattening happens at ingest instead of in the expression.
    tags_text    TEXT        NOT NULL DEFAULT '',
    headings     TEXT[]      NOT NULL DEFAULT '{}',
    mtime        DOUBLE PRECISION NOT NULL DEFAULT 0,
    size         BIGINT      NOT NULL DEFAULT 0,
    degree       INTEGER     NOT NULL DEFAULT 0,
    -- Content hash, so an editor that rewrites a file without changing a byte
    -- of it costs one comparison instead of a reparse and a reindex.
    content_hash BYTEA       NOT NULL,
    -- Stable position on its ribbon, derived from brain_id + rel_path rather
    -- than from an array index. This is what lets one note change without
    -- reshuffling the galaxy around it.
    slot         DOUBLE PRECISION NOT NULL DEFAULT 0,
    indexed_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (brain_id, rel_path)
);

CREATE INDEX IF NOT EXISTS note_brain_idx  ON note (brain_id);
CREATE INDEX IF NOT EXISTS note_mtime_idx  ON note (mtime DESC);
CREATE INDEX IF NOT EXISTS note_degree_idx ON note (brain_id, degree DESC);
-- Link resolution matches on the basename and on the title, both folded.
CREATE INDEX IF NOT EXISTS note_title_idx  ON note (lower(title));

-- Columns added after a database already existed. CREATE TABLE IF NOT EXISTS
-- silently skips an existing table, so anything added later needs its own
-- ALTER or a database created by an older build never gains it.
ALTER TABLE note ADD COLUMN IF NOT EXISTS tags_text TEXT NOT NULL DEFAULT '';
ALTER TABLE note ADD COLUMN IF NOT EXISTS headings TEXT[] NOT NULL DEFAULT '{}';
ALTER TABLE note ADD COLUMN IF NOT EXISTS slot DOUBLE PRECISION NOT NULL DEFAULT 0;

-- Full text. Title is weighted above body so a note called "Darkwater" beats
-- one that merely mentions it; tags and folder sit between.
-- 'english'::regconfig is spelled out because the one-argument to_tsvector
-- reads default_text_search_config at runtime and is therefore not immutable.
ALTER TABLE note ADD COLUMN IF NOT EXISTS tsv tsvector
    GENERATED ALWAYS AS (
        setweight(to_tsvector('english'::regconfig, coalesce(title, '')), 'A') ||
        setweight(to_tsvector('english'::regconfig, coalesce(tags_text, '')), 'B') ||
        setweight(to_tsvector('english'::regconfig, coalesce(source, '')), 'C') ||
        setweight(to_tsvector('english'::regconfig, coalesce(body, '')), 'D')
    ) STORED;

CREATE INDEX IF NOT EXISTS note_tsv_idx ON note USING GIN (tsv);

-- Trigram matching on titles, so a wikilink whose target is slightly off still
-- finds its note. Postgres ships this as an extension rather than a builtin.
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE INDEX IF NOT EXISTS note_title_trgm_idx ON note USING GIN (title gin_trgm_ops);

CREATE TABLE IF NOT EXISTS link (
    src_id BIGINT NOT NULL REFERENCES note(id) ON DELETE CASCADE,
    dst_id BIGINT NOT NULL REFERENCES note(id) ON DELETE CASCADE,
    PRIMARY KEY (src_id, dst_id)
);
CREATE INDEX IF NOT EXISTS link_dst_idx ON link (dst_id);

-- Every target a note points at, resolved or not. The unresolved ones are kept
-- because in a vault they are usually notes you meant to write.
CREATE TABLE IF NOT EXISTS link_target (
    id       BIGSERIAL PRIMARY KEY,
    src_id   BIGINT NOT NULL REFERENCES note(id) ON DELETE CASCADE,
    target   TEXT   NOT NULL,
    resolved BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX IF NOT EXISTS link_target_src_idx ON link_target (src_id);
CREATE INDEX IF NOT EXISTS link_target_unresolved_idx
    ON link_target (src_id) WHERE NOT resolved;

-- Precomputed geometry, one row per brain. Invalidated by revision, not by
-- time, so a rebuild happens exactly when the notes underneath it changed.
CREATE TABLE IF NOT EXISTS graph_cache (
    brain_id   TEXT PRIMARY KEY REFERENCES brain(id) ON DELETE CASCADE,
    revision   BIGINT NOT NULL,
    node_count INTEGER NOT NULL,
    edge_count INTEGER NOT NULL,
    -- Packed little-endian buffers the browser uploads straight into GPU
    -- attributes; see layout.rs for the exact framing.
    geometry   BYTEA  NOT NULL,
    meta       JSONB  NOT NULL,
    built_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
