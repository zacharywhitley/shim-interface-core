-- v3 -> v4 migration (B4). Additive-only.
--
-- Mirrors the B0 tracking columns onto the five catalog tables
-- that were catalogued but had no lineage: `column_types`,
-- `operators`, `cast_rewrites`, `spatial_indexes`,
-- `preprocessor_patterns`. Same nine-column shape as the four
-- function tables minus `implementation_hash` — these entities
-- don't map to a single source module; the signature hash IS
-- the identity.
--
-- Also:
--   * BEFORE-INSERT status-enum triggers for each new table.
--   * Rebuilds `status_summary_per_extension` to union the new
--     tables into the roll-up.
--   * Adds `status_summary_per_kind` — per-kind status split so
--     funcs-md-gen can render one line per entity kind.
--   * Broadens `leaf_coverage`'s "verified?" bit to look at
--     aggregate / table_function / window_function status too,
--     via a COALESCE (rather than just scalars).
--
-- Runs under a single transaction. Idempotency comes from
-- `CREATE ... IF NOT EXISTS` on new triggers/views, the DROP
-- VIEW / CREATE VIEW pair on the two rebuilt views, and the
-- migration function being gated on `user_version = 3` so it
-- can't double-apply in practice.

BEGIN;

ALTER TABLE column_types ADD COLUMN first_seen_upstream_version       TEXT;
ALTER TABLE column_types ADD COLUMN last_seen_upstream_version        TEXT;
ALTER TABLE column_types ADD COLUMN deprecated_in_upstream_version    TEXT;
ALTER TABLE column_types ADD COLUMN signature_hash                    TEXT;
ALTER TABLE column_types ADD COLUMN status                            TEXT NOT NULL DEFAULT 'implemented_unverified';
ALTER TABLE column_types ADD COLUMN last_verified_upstream_version    TEXT;
ALTER TABLE column_types ADD COLUMN last_verified_signature_hash      TEXT;
ALTER TABLE column_types ADD COLUMN last_verified_at                  TEXT;
ALTER TABLE column_types ADD COLUMN notes                             TEXT;

ALTER TABLE operators ADD COLUMN first_seen_upstream_version       TEXT;
ALTER TABLE operators ADD COLUMN last_seen_upstream_version        TEXT;
ALTER TABLE operators ADD COLUMN deprecated_in_upstream_version    TEXT;
ALTER TABLE operators ADD COLUMN signature_hash                    TEXT;
ALTER TABLE operators ADD COLUMN status                            TEXT NOT NULL DEFAULT 'implemented_unverified';
ALTER TABLE operators ADD COLUMN last_verified_upstream_version    TEXT;
ALTER TABLE operators ADD COLUMN last_verified_signature_hash      TEXT;
ALTER TABLE operators ADD COLUMN last_verified_at                  TEXT;
ALTER TABLE operators ADD COLUMN notes                             TEXT;

ALTER TABLE cast_rewrites ADD COLUMN first_seen_upstream_version       TEXT;
ALTER TABLE cast_rewrites ADD COLUMN last_seen_upstream_version        TEXT;
ALTER TABLE cast_rewrites ADD COLUMN deprecated_in_upstream_version    TEXT;
ALTER TABLE cast_rewrites ADD COLUMN signature_hash                    TEXT;
ALTER TABLE cast_rewrites ADD COLUMN status                            TEXT NOT NULL DEFAULT 'implemented_unverified';
ALTER TABLE cast_rewrites ADD COLUMN last_verified_upstream_version    TEXT;
ALTER TABLE cast_rewrites ADD COLUMN last_verified_signature_hash      TEXT;
ALTER TABLE cast_rewrites ADD COLUMN last_verified_at                  TEXT;
ALTER TABLE cast_rewrites ADD COLUMN notes                             TEXT;

ALTER TABLE spatial_indexes ADD COLUMN first_seen_upstream_version       TEXT;
ALTER TABLE spatial_indexes ADD COLUMN last_seen_upstream_version        TEXT;
ALTER TABLE spatial_indexes ADD COLUMN deprecated_in_upstream_version    TEXT;
ALTER TABLE spatial_indexes ADD COLUMN signature_hash                    TEXT;
ALTER TABLE spatial_indexes ADD COLUMN status                            TEXT NOT NULL DEFAULT 'implemented_unverified';
ALTER TABLE spatial_indexes ADD COLUMN last_verified_upstream_version    TEXT;
ALTER TABLE spatial_indexes ADD COLUMN last_verified_signature_hash      TEXT;
ALTER TABLE spatial_indexes ADD COLUMN last_verified_at                  TEXT;
ALTER TABLE spatial_indexes ADD COLUMN notes                             TEXT;

ALTER TABLE preprocessor_patterns ADD COLUMN first_seen_upstream_version       TEXT;
ALTER TABLE preprocessor_patterns ADD COLUMN last_seen_upstream_version        TEXT;
ALTER TABLE preprocessor_patterns ADD COLUMN deprecated_in_upstream_version    TEXT;
ALTER TABLE preprocessor_patterns ADD COLUMN signature_hash                    TEXT;
ALTER TABLE preprocessor_patterns ADD COLUMN status                            TEXT NOT NULL DEFAULT 'implemented_unverified';
ALTER TABLE preprocessor_patterns ADD COLUMN last_verified_upstream_version    TEXT;
ALTER TABLE preprocessor_patterns ADD COLUMN last_verified_signature_hash      TEXT;
ALTER TABLE preprocessor_patterns ADD COLUMN last_verified_at                  TEXT;
ALTER TABLE preprocessor_patterns ADD COLUMN notes                             TEXT;

CREATE TRIGGER IF NOT EXISTS trg_column_types_status_enum
    BEFORE INSERT ON column_types
    FOR EACH ROW WHEN NEW.status NOT IN
        ('implemented_unverified','implemented_verified',
         'deprecated','unimplemented','skip')
    BEGIN SELECT RAISE(ABORT, 'invalid status'); END;

CREATE TRIGGER IF NOT EXISTS trg_operators_status_enum
    BEFORE INSERT ON operators
    FOR EACH ROW WHEN NEW.status NOT IN
        ('implemented_unverified','implemented_verified',
         'deprecated','unimplemented','skip')
    BEGIN SELECT RAISE(ABORT, 'invalid status'); END;

CREATE TRIGGER IF NOT EXISTS trg_cast_rewrites_status_enum
    BEFORE INSERT ON cast_rewrites
    FOR EACH ROW WHEN NEW.status NOT IN
        ('implemented_unverified','implemented_verified',
         'deprecated','unimplemented','skip')
    BEGIN SELECT RAISE(ABORT, 'invalid status'); END;

CREATE TRIGGER IF NOT EXISTS trg_spatial_indexes_status_enum
    BEFORE INSERT ON spatial_indexes
    FOR EACH ROW WHEN NEW.status NOT IN
        ('implemented_unverified','implemented_verified',
         'deprecated','unimplemented','skip')
    BEGIN SELECT RAISE(ABORT, 'invalid status'); END;

CREATE TRIGGER IF NOT EXISTS trg_preprocessor_patterns_status_enum
    BEFORE INSERT ON preprocessor_patterns
    FOR EACH ROW WHEN NEW.status NOT IN
        ('implemented_unverified','implemented_verified',
         'deprecated','unimplemented','skip')
    BEGIN SELECT RAISE(ABORT, 'invalid status'); END;

-- Rebuild `status_summary_per_extension` to union the five new
-- tables in.
DROP VIEW IF EXISTS status_summary_per_extension;
CREATE VIEW status_summary_per_extension AS
    SELECT extension, status, COUNT(*) AS n
    FROM (
        SELECT extension, status FROM scalars
        UNION ALL SELECT extension, status FROM aggregates
        UNION ALL SELECT extension, status FROM table_functions
        UNION ALL SELECT extension, status FROM window_functions
        UNION ALL SELECT extension, status FROM column_types
        UNION ALL SELECT extension, status FROM operators
        UNION ALL SELECT extension, status FROM cast_rewrites
        UNION ALL SELECT extension, status FROM spatial_indexes
        UNION ALL SELECT extension, status FROM preprocessor_patterns
    )
    GROUP BY extension, status;

-- New view: per-kind status split. Consumed by funcs-md-gen's
-- new Types / Operators / Casts / Spatial Indexes / Preprocessor
-- Patterns sections.
CREATE VIEW IF NOT EXISTS status_summary_per_kind AS
    SELECT extension, 'scalar' AS kind, status, COUNT(*) AS n
        FROM scalars               GROUP BY extension, status
    UNION ALL
    SELECT extension, 'aggregate',          status, COUNT(*)
        FROM aggregates            GROUP BY extension, status
    UNION ALL
    SELECT extension, 'table_function',     status, COUNT(*)
        FROM table_functions       GROUP BY extension, status
    UNION ALL
    SELECT extension, 'window_function',    status, COUNT(*)
        FROM window_functions      GROUP BY extension, status
    UNION ALL
    SELECT extension, 'column_type',        status, COUNT(*)
        FROM column_types          GROUP BY extension, status
    UNION ALL
    SELECT extension, 'operator',           status, COUNT(*)
        FROM operators             GROUP BY extension, status
    UNION ALL
    SELECT extension, 'cast_rewrite',       status, COUNT(*)
        FROM cast_rewrites         GROUP BY extension, status
    UNION ALL
    SELECT extension, 'spatial_index',      status, COUNT(*)
        FROM spatial_indexes       GROUP BY extension, status
    UNION ALL
    SELECT extension, 'preprocessor_pattern', status, COUNT(*)
        FROM preprocessor_patterns GROUP BY extension, status;

-- Broaden `leaf_coverage.verified_functions` to consider
-- aggregate / table_function / window_function rows too, not
-- just scalars.
DROP VIEW IF EXISTS leaf_coverage;
CREATE VIEW leaf_coverage AS
    SELECT
        tc.extension,
        COALESCE(
            (SELECT j.value FROM json_each(tc.tags_json) AS j
                WHERE j.value LIKE 'leaf:%' LIMIT 1),
            json_extract(tc.tags_json, '$[0]')
        ) AS leaf,
        COUNT(DISTINCT tc.function_name) AS functions_with_cases,
        SUM(CASE
            WHEN COALESCE(s.status, a.status, tf.status, wf.status)
                = 'implemented_verified' THEN 1 ELSE 0
        END) AS verified_functions,
        ROUND(
            100.0 *
            SUM(CASE
                WHEN COALESCE(s.status, a.status, tf.status, wf.status)
                    = 'implemented_verified' THEN 1 ELSE 0
            END)
            / COUNT(DISTINCT tc.function_name),
            1
        ) AS coverage_pct
    FROM test_cases tc
    LEFT JOIN scalars s
      ON s.extension = tc.extension AND s.name = tc.function_name
    LEFT JOIN aggregates a
      ON a.extension = tc.extension AND a.name = tc.function_name
    LEFT JOIN table_functions tf
      ON tf.extension = tc.extension AND tf.name = tc.function_name
    LEFT JOIN window_functions wf
      ON wf.extension = tc.extension AND wf.name = tc.function_name
    GROUP BY tc.extension, leaf;

PRAGMA user_version = 4;

COMMIT;
