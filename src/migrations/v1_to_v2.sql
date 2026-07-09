-- v1 -> v2 migration. Additive only:
--   - 12 metadata columns on each of the 4 function tables
--     (`interface`, upstream-version tracking, hashes,
--     status/verification, notes).
--   - 4 new tables: `upstream_versions`,
--     `function_dependencies`, `test_cases`, `test_runs`.
--   - Supporting indexes and status-enum triggers.
--   - `function_status_summary` and
--     `function_reverse_transitive` views.
--
-- Runs under a single transaction. Idempotency comes from
-- `CREATE ... IF NOT EXISTS` on new tables/indexes/triggers/views
-- and from the ADD-COLUMN failing loudly if anyone re-runs
-- against a partially-migrated DB. The migration function is
-- gated on `user_version = 1` so double-application isn't a
-- concern in practice.

BEGIN;

ALTER TABLE scalars ADD COLUMN interface                         TEXT;
ALTER TABLE scalars ADD COLUMN first_seen_upstream_version       TEXT;
ALTER TABLE scalars ADD COLUMN last_seen_upstream_version        TEXT;
ALTER TABLE scalars ADD COLUMN deprecated_in_upstream_version    TEXT;
ALTER TABLE scalars ADD COLUMN signature_hash                    TEXT;
ALTER TABLE scalars ADD COLUMN implementation_hash               TEXT;
ALTER TABLE scalars ADD COLUMN status                            TEXT NOT NULL DEFAULT 'implemented_unverified';
ALTER TABLE scalars ADD COLUMN last_verified_upstream_version    TEXT;
ALTER TABLE scalars ADD COLUMN last_verified_signature_hash      TEXT;
ALTER TABLE scalars ADD COLUMN last_verified_implementation_hash TEXT;
ALTER TABLE scalars ADD COLUMN last_verified_at                  TEXT;
ALTER TABLE scalars ADD COLUMN notes                             TEXT;

ALTER TABLE aggregates ADD COLUMN interface                         TEXT;
ALTER TABLE aggregates ADD COLUMN first_seen_upstream_version       TEXT;
ALTER TABLE aggregates ADD COLUMN last_seen_upstream_version        TEXT;
ALTER TABLE aggregates ADD COLUMN deprecated_in_upstream_version    TEXT;
ALTER TABLE aggregates ADD COLUMN signature_hash                    TEXT;
ALTER TABLE aggregates ADD COLUMN implementation_hash               TEXT;
ALTER TABLE aggregates ADD COLUMN status                            TEXT NOT NULL DEFAULT 'implemented_unverified';
ALTER TABLE aggregates ADD COLUMN last_verified_upstream_version    TEXT;
ALTER TABLE aggregates ADD COLUMN last_verified_signature_hash      TEXT;
ALTER TABLE aggregates ADD COLUMN last_verified_implementation_hash TEXT;
ALTER TABLE aggregates ADD COLUMN last_verified_at                  TEXT;
ALTER TABLE aggregates ADD COLUMN notes                             TEXT;

ALTER TABLE table_functions ADD COLUMN interface                         TEXT;
ALTER TABLE table_functions ADD COLUMN first_seen_upstream_version       TEXT;
ALTER TABLE table_functions ADD COLUMN last_seen_upstream_version        TEXT;
ALTER TABLE table_functions ADD COLUMN deprecated_in_upstream_version    TEXT;
ALTER TABLE table_functions ADD COLUMN signature_hash                    TEXT;
ALTER TABLE table_functions ADD COLUMN implementation_hash               TEXT;
ALTER TABLE table_functions ADD COLUMN status                            TEXT NOT NULL DEFAULT 'implemented_unverified';
ALTER TABLE table_functions ADD COLUMN last_verified_upstream_version    TEXT;
ALTER TABLE table_functions ADD COLUMN last_verified_signature_hash      TEXT;
ALTER TABLE table_functions ADD COLUMN last_verified_implementation_hash TEXT;
ALTER TABLE table_functions ADD COLUMN last_verified_at                  TEXT;
ALTER TABLE table_functions ADD COLUMN notes                             TEXT;

ALTER TABLE window_functions ADD COLUMN interface                         TEXT;
ALTER TABLE window_functions ADD COLUMN first_seen_upstream_version       TEXT;
ALTER TABLE window_functions ADD COLUMN last_seen_upstream_version        TEXT;
ALTER TABLE window_functions ADD COLUMN deprecated_in_upstream_version    TEXT;
ALTER TABLE window_functions ADD COLUMN signature_hash                    TEXT;
ALTER TABLE window_functions ADD COLUMN implementation_hash               TEXT;
ALTER TABLE window_functions ADD COLUMN status                            TEXT NOT NULL DEFAULT 'implemented_unverified';
ALTER TABLE window_functions ADD COLUMN last_verified_upstream_version    TEXT;
ALTER TABLE window_functions ADD COLUMN last_verified_signature_hash      TEXT;
ALTER TABLE window_functions ADD COLUMN last_verified_implementation_hash TEXT;
ALTER TABLE window_functions ADD COLUMN last_verified_at                  TEXT;
ALTER TABLE window_functions ADD COLUMN notes                             TEXT;

CREATE TABLE IF NOT EXISTS upstream_versions (
    extension              TEXT NOT NULL,
    version                TEXT NOT NULL,
    released_at            TEXT,
    ingested_at            TEXT NOT NULL,
    ingested_from_commit   TEXT,
    scalar_count           INTEGER,
    aggregate_count        INTEGER,
    table_function_count   INTEGER,
    window_function_count  INTEGER,
    notes                  TEXT,
    PRIMARY KEY (extension, version),
    FOREIGN KEY (extension) REFERENCES extensions(name)
);

CREATE TABLE IF NOT EXISTS function_dependencies (
    extension          TEXT NOT NULL,
    caller_name        TEXT NOT NULL,
    caller_kind        TEXT NOT NULL,
    callee_extension   TEXT NOT NULL,
    callee_name        TEXT NOT NULL,
    callee_kind        TEXT NOT NULL,
    edge_kind          TEXT NOT NULL,
    source_hint        TEXT,
    PRIMARY KEY (extension, caller_name, callee_extension, callee_name, edge_kind)
);
CREATE INDEX IF NOT EXISTS idx_fdep_callee
    ON function_dependencies(callee_extension, callee_name);
CREATE INDEX IF NOT EXISTS idx_fdep_caller
    ON function_dependencies(extension, caller_name);

CREATE TABLE IF NOT EXISTS test_cases (
    extension       TEXT NOT NULL,
    function_name   TEXT NOT NULL,
    case_name       TEXT NOT NULL,
    source          TEXT NOT NULL,
    source_path     TEXT,
    sql_inline      TEXT,
    expected        TEXT,
    tags_json       TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (extension, function_name, case_name)
);
CREATE INDEX IF NOT EXISTS idx_test_cases_fn
    ON test_cases(extension, function_name);

CREATE TABLE IF NOT EXISTS test_runs (
    run_id                INTEGER PRIMARY KEY AUTOINCREMENT,
    extension             TEXT NOT NULL,
    function_name         TEXT NOT NULL,
    case_name             TEXT NOT NULL,
    status                TEXT NOT NULL,
    actual                TEXT,
    duration_ms           INTEGER,
    host_version          TEXT,
    provider_wasm_hash    TEXT,
    bridge_wasm_hash      TEXT,
    upstream_version      TEXT,
    ran_at                TEXT NOT NULL,
    FOREIGN KEY (extension, function_name, case_name)
        REFERENCES test_cases(extension, function_name, case_name)
);
CREATE INDEX IF NOT EXISTS idx_test_runs_fn_time
    ON test_runs(function_name, ran_at DESC);
CREATE INDEX IF NOT EXISTS idx_test_runs_status
    ON test_runs(status, ran_at DESC);

CREATE TRIGGER IF NOT EXISTS trg_scalars_status_enum
    BEFORE INSERT ON scalars
    FOR EACH ROW WHEN NEW.status NOT IN
        ('implemented_unverified','implemented_verified',
         'deprecated','unimplemented','skip')
    BEGIN SELECT RAISE(ABORT, 'invalid status'); END;

CREATE TRIGGER IF NOT EXISTS trg_aggregates_status_enum
    BEFORE INSERT ON aggregates
    FOR EACH ROW WHEN NEW.status NOT IN
        ('implemented_unverified','implemented_verified',
         'deprecated','unimplemented','skip')
    BEGIN SELECT RAISE(ABORT, 'invalid status'); END;

CREATE TRIGGER IF NOT EXISTS trg_table_functions_status_enum
    BEFORE INSERT ON table_functions
    FOR EACH ROW WHEN NEW.status NOT IN
        ('implemented_unverified','implemented_verified',
         'deprecated','unimplemented','skip')
    BEGIN SELECT RAISE(ABORT, 'invalid status'); END;

CREATE TRIGGER IF NOT EXISTS trg_window_functions_status_enum
    BEFORE INSERT ON window_functions
    FOR EACH ROW WHEN NEW.status NOT IN
        ('implemented_unverified','implemented_verified',
         'deprecated','unimplemented','skip')
    BEGIN SELECT RAISE(ABORT, 'invalid status'); END;

CREATE VIEW IF NOT EXISTS function_status_summary AS
SELECT extension, 'scalar' AS kind, status, COUNT(*) AS n
    FROM scalars           GROUP BY extension, status
UNION ALL
SELECT extension, 'aggregate',       status, COUNT(*)
    FROM aggregates        GROUP BY extension, status
UNION ALL
SELECT extension, 'table_function',  status, COUNT(*)
    FROM table_functions   GROUP BY extension, status
UNION ALL
SELECT extension, 'window_function', status, COUNT(*)
    FROM window_functions  GROUP BY extension, status;

CREATE VIEW IF NOT EXISTS function_reverse_transitive AS
WITH RECURSIVE
    rev(root_extension, root_callee, extension, caller_name, depth) AS (
        SELECT callee_extension, callee_name, extension, caller_name, 1
            FROM function_dependencies
        UNION
        SELECT r.root_extension, r.root_callee,
               fd.extension, fd.caller_name, r.depth + 1
            FROM function_dependencies fd
            JOIN rev r
              ON fd.callee_extension = r.extension
             AND fd.callee_name      = r.caller_name
            WHERE r.depth < 16
    )
SELECT root_extension, root_callee, extension, caller_name, MIN(depth) AS depth
    FROM rev
    GROUP BY root_extension, root_callee, extension, caller_name;

PRAGMA user_version = 2;

COMMIT;
