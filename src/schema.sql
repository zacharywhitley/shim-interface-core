-- Schema for the shim-interface SQLite database (v4).
--
-- Every row's `extension` is the shim's WIT identity name
-- (`"postgis"` / `"mobilitydb"` etc.); composite keys are
-- `(extension, name)` so a single database can hold multiple
-- shims side-by-side. Snapshot diffs work by ATTACHing two
-- databases and comparing.
--
-- v2 (B0) additions:
--   - `interface`, `first_seen_upstream_version`,
--     `last_seen_upstream_version`, `deprecated_in_upstream_version`,
--     `signature_hash`, `implementation_hash`, `status` (with default
--     `implemented_unverified`), plus three `last_verified_*` columns
--     and a free-form `notes` slot on the four function tables.
--   - New tables `upstream_versions`, `function_dependencies`,
--     `test_cases`, `test_runs` with supporting indexes.
--   - Soft-enum triggers guarding `status` values against
--     hand-editing.
--
-- v3 (B3) additions:
--   - Three coverage views feeding the auto-generated FUNCTIONS.md
--     surfaces: `status_summary_per_extension`, `leaf_coverage`,
--     and `verification_freshness`. Views only — no data reshape.
--
-- v4 (B4) additions:
--   - Lineage tracking mirrored onto five more catalog tables:
--     `column_types`, `operators`, `cast_rewrites`,
--     `spatial_indexes`, `preprocessor_patterns`. Each gains
--     `first_seen_upstream_version`, `last_seen_upstream_version`,
--     `deprecated_in_upstream_version`, `signature_hash`, `status`
--     (with default `implemented_unverified`), plus three
--     `last_verified_*` columns and a `notes` slot. No
--     `implementation_hash` — these entities don't map to a source
--     module; `signature_hash` is their identity.
--   - `status_summary_per_extension` rolls up all nine kinds now.
--   - Soft-enum triggers guarding `status` on the five new tables.
--
-- `PRAGMA user_version` is tagged to 4 at the tail so the migration
-- code (see `shim-interface-core::migrations`) can distinguish a
-- fresh `open_fresh` DB from a legacy v1/v2/v3 one that needs backfill.

CREATE TABLE IF NOT EXISTS extensions (
    name TEXT PRIMARY KEY,
    version TEXT NOT NULL,
    api_version TEXT,
    wasm_path TEXT NOT NULL,
    wasm_blake3 TEXT NOT NULL,
    extracted_at TEXT NOT NULL  -- RFC3339
);

CREATE TABLE IF NOT EXISTS scalars (
    extension TEXT NOT NULL,
    name TEXT NOT NULL,
    param_types_json TEXT NOT NULL,  -- JSON array of arrays of type-name strings
    return_type TEXT NOT NULL,
    is_deterministic INTEGER NOT NULL,
    propagates_null INTEGER NOT NULL,
    interface TEXT,
    first_seen_upstream_version TEXT,
    last_seen_upstream_version TEXT,
    deprecated_in_upstream_version TEXT,
    signature_hash TEXT,
    implementation_hash TEXT,
    status TEXT NOT NULL DEFAULT 'implemented_unverified',
    last_verified_upstream_version TEXT,
    last_verified_signature_hash TEXT,
    last_verified_implementation_hash TEXT,
    last_verified_at TEXT,
    notes TEXT,
    PRIMARY KEY (extension, name)
);

-- Scalar function aliases.
--
-- Doctrine note (2026-06-23 investigation): scalar shims expose
-- aliases two ways. PostGIS uses `ScalarFunctionDef::aliases()`,
-- returning a non-empty Vec from one canonical impl -- those
-- rows land here. MobilityDB instead pushes each alias as its
-- own canonical `(name, Kind)` dispatch-table entry; its
-- `aliases()` returns empty, so this table is empty for that
-- shim (all 1548 mobilitydb scalars sit in `scalars` only,
-- including its ~275 internal aliases). Both choices are
-- correct against the trait -- the difference is bookkeeping
-- shape, not behavior. Tooling computing "total function
-- names" should sum `scalars.count + scalar_aliases.count` to
-- be fair across both shapes.
CREATE TABLE IF NOT EXISTS scalar_aliases (
    extension TEXT NOT NULL,
    canonical TEXT NOT NULL,
    alias TEXT NOT NULL,
    PRIMARY KEY (extension, alias),
    FOREIGN KEY (extension, canonical) REFERENCES scalars(extension, name)
);

CREATE TABLE IF NOT EXISTS aggregates (
    extension TEXT NOT NULL,
    name TEXT NOT NULL,
    param_types_json TEXT NOT NULL,
    supports_grouped INTEGER NOT NULL,
    supports_partial INTEGER NOT NULL,
    is_order_sensitive INTEGER NOT NULL,
    accepts_config INTEGER NOT NULL,
    config_arg_indices_json TEXT NOT NULL,
    interface TEXT,
    first_seen_upstream_version TEXT,
    last_seen_upstream_version TEXT,
    deprecated_in_upstream_version TEXT,
    signature_hash TEXT,
    implementation_hash TEXT,
    status TEXT NOT NULL DEFAULT 'implemented_unverified',
    last_verified_upstream_version TEXT,
    last_verified_signature_hash TEXT,
    last_verified_implementation_hash TEXT,
    last_verified_at TEXT,
    notes TEXT,
    PRIMARY KEY (extension, name)
);

CREATE TABLE IF NOT EXISTS aggregate_aliases (
    extension TEXT NOT NULL,
    canonical TEXT NOT NULL,
    alias TEXT NOT NULL,
    PRIMARY KEY (extension, alias),
    FOREIGN KEY (extension, canonical) REFERENCES aggregates(extension, name)
);

CREATE TABLE IF NOT EXISTS table_functions (
    extension TEXT NOT NULL,
    name TEXT NOT NULL,
    param_types_json TEXT NOT NULL,
    -- JSON array of [column_name, type_token] pairs describing the
    -- output row schema (e.g. `[["timestamp","int64"],["value","float64"]]`).
    -- Populated at extraction time from TableFunctionDef::output_schema.
    -- Optional for backward-compat with pre-B5 extractions; downstream
    -- codegen falls back to a single `result BLOB` column when NULL.
    output_columns_json TEXT,
    interface TEXT,
    first_seen_upstream_version TEXT,
    last_seen_upstream_version TEXT,
    deprecated_in_upstream_version TEXT,
    signature_hash TEXT,
    implementation_hash TEXT,
    status TEXT NOT NULL DEFAULT 'implemented_unverified',
    last_verified_upstream_version TEXT,
    last_verified_signature_hash TEXT,
    last_verified_implementation_hash TEXT,
    last_verified_at TEXT,
    notes TEXT,
    PRIMARY KEY (extension, name)
);

CREATE TABLE IF NOT EXISTS table_function_aliases (
    extension TEXT NOT NULL,
    canonical TEXT NOT NULL,
    alias TEXT NOT NULL,
    PRIMARY KEY (extension, alias),
    FOREIGN KEY (extension, canonical) REFERENCES table_functions(extension, name)
);

CREATE TABLE IF NOT EXISTS window_functions (
    extension TEXT NOT NULL,
    name TEXT NOT NULL,
    param_types_json TEXT NOT NULL,
    interface TEXT,
    first_seen_upstream_version TEXT,
    last_seen_upstream_version TEXT,
    deprecated_in_upstream_version TEXT,
    signature_hash TEXT,
    implementation_hash TEXT,
    status TEXT NOT NULL DEFAULT 'implemented_unverified',
    last_verified_upstream_version TEXT,
    last_verified_signature_hash TEXT,
    last_verified_implementation_hash TEXT,
    last_verified_at TEXT,
    notes TEXT,
    PRIMARY KEY (extension, name)
);

CREATE TABLE IF NOT EXISTS window_function_aliases (
    extension TEXT NOT NULL,
    canonical TEXT NOT NULL,
    alias TEXT NOT NULL,
    PRIMARY KEY (extension, alias),
    FOREIGN KEY (extension, canonical) REFERENCES window_functions(extension, name)
);

CREATE TABLE IF NOT EXISTS column_types (
    extension TEXT NOT NULL,
    type_id INTEGER NOT NULL,
    type_name TEXT NOT NULL,
    storage_size INTEGER NOT NULL,
    cast_from_json TEXT NOT NULL,
    cast_to_json TEXT NOT NULL,
    -- v4 (B4) lineage tracking columns. Same shape as the four
    -- function tables minus `implementation_hash` (types don't
    -- map to a single source module; the signature IS the identity).
    first_seen_upstream_version TEXT,
    last_seen_upstream_version TEXT,
    deprecated_in_upstream_version TEXT,
    signature_hash TEXT,
    status TEXT NOT NULL DEFAULT 'implemented_unverified',
    last_verified_upstream_version TEXT,
    last_verified_signature_hash TEXT,
    last_verified_at TEXT,
    notes TEXT,
    PRIMARY KEY (extension, type_id)
);

CREATE TABLE IF NOT EXISTS operators (
    extension TEXT NOT NULL,
    symbol TEXT NOT NULL,
    lhs_type_id INTEGER,
    rhs_type_id INTEGER,
    function_name TEXT NOT NULL,
    -- v4 (B4) lineage tracking columns.
    first_seen_upstream_version TEXT,
    last_seen_upstream_version TEXT,
    deprecated_in_upstream_version TEXT,
    signature_hash TEXT,
    status TEXT NOT NULL DEFAULT 'implemented_unverified',
    last_verified_upstream_version TEXT,
    last_verified_signature_hash TEXT,
    last_verified_at TEXT,
    notes TEXT,
    PRIMARY KEY (extension, symbol, lhs_type_id, rhs_type_id)
);

CREATE TABLE IF NOT EXISTS cast_rewrites (
    extension TEXT NOT NULL,
    target_type TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    function_name TEXT NOT NULL,
    source_fn_hint TEXT NOT NULL,
    -- Extension-namespaced source-side type id (or 0 when the
    -- source shape is not discriminated by type -- e.g. bytea-fed
    -- `st_geomfromwkb` accepts any bit pattern). Populated from the
    -- WIT-side `cast-rewrite.source-type-id` field (#798).
    source_type_id INTEGER NOT NULL DEFAULT 0,
    -- v4 (B4) lineage tracking columns.
    first_seen_upstream_version TEXT,
    last_seen_upstream_version TEXT,
    deprecated_in_upstream_version TEXT,
    signature_hash TEXT,
    status TEXT NOT NULL DEFAULT 'implemented_unverified',
    last_verified_upstream_version TEXT,
    last_verified_signature_hash TEXT,
    last_verified_at TEXT,
    notes TEXT,
    -- `source_fn_hint` and `source_type_id` are part of the PK so
    -- distinct source-side rewrites that share a (target_type,
    -- source_kind) key don't collide under `INSERT OR IGNORE`.
    -- PostGIS advertises many casts that discriminate by the
    -- source-side function (box2d::geometry vs. box3d::geometry,
    -- both under `any`) -- those separate via `source_fn_hint`. It
    -- also advertises identity + PostgreSQL-native + topogeom +
    -- raster rewrites that all target `geometry` under
    -- `(source_kind=any, source_fn_hint="")`; those separate via
    -- `source_type_id`. Before #798 the narrower PK dropped 7 of
    -- PostGIS' 39 cast rewrites at INSERT OR IGNORE time; #788
    -- had already caught the source_fn_hint axis but the
    -- source-side type-id axis needed adding.
    PRIMARY KEY (extension, target_type, source_kind, source_fn_hint, source_type_id)
);

CREATE TABLE IF NOT EXISTS preprocessor_patterns (
    extension TEXT NOT NULL,
    op_token TEXT NOT NULL,
    function_name TEXT NOT NULL,
    -- v4 (B4) lineage tracking columns.
    first_seen_upstream_version TEXT,
    last_seen_upstream_version TEXT,
    deprecated_in_upstream_version TEXT,
    signature_hash TEXT,
    status TEXT NOT NULL DEFAULT 'implemented_unverified',
    last_verified_upstream_version TEXT,
    last_verified_signature_hash TEXT,
    last_verified_at TEXT,
    notes TEXT,
    PRIMARY KEY (extension, op_token)
);

CREATE TABLE IF NOT EXISTS system_catalog_tables (
    extension TEXT NOT NULL,
    catalog_name TEXT NOT NULL,
    table_name TEXT NOT NULL,
    columns_json TEXT NOT NULL,
    PRIMARY KEY (extension, catalog_name, table_name)
);

-- Spatial-index implementations.
--
-- TWO sources feed this table:
--   1. `index-plugin/index@1.0.0` -- per-extension callback,
--      surfaces through `ExtensionTarget::register_index_builder`.
--      `type_id` is the WIT-declared id; `capabilities_json` is
--      null because that interface doesn't carry capability flags.
--   2. `spatial-index-plugin/spatial-index@1.0.0` -- process-
--      global, surfaces through `extract_spatial_index_metadata`.
--      `type_id` is 0 (sentinel -- that interface doesn't expose
--      ids, it routes by alias); `capabilities_json` carries
--      `{knn, within_distance, within_distance_wkb,
--      update_after_build}` booleans.
--
-- PostGIS uses path #2 exclusively; MobilityDB uses #1 (its
-- stindex). The 2026-06-23 investigation found that without
-- the path-#2 drain, PostGIS extractions reported zero indexes.
CREATE TABLE IF NOT EXISTS spatial_indexes (
    extension TEXT NOT NULL,
    name TEXT NOT NULL,
    type_id INTEGER NOT NULL,
    capabilities_json TEXT,
    -- v4 (B4) lineage tracking columns.
    first_seen_upstream_version TEXT,
    last_seen_upstream_version TEXT,
    deprecated_in_upstream_version TEXT,
    signature_hash TEXT,
    status TEXT NOT NULL DEFAULT 'implemented_unverified',
    last_verified_upstream_version TEXT,
    last_verified_signature_hash TEXT,
    last_verified_at TEXT,
    notes TEXT,
    PRIMARY KEY (extension, name)
);

-- v2 (B0): upstream_versions -- one row per (extension, upstream release)
-- ingested so provenance and lineage queries can trace a signature/
-- implementation hash back to the specific tree it was extracted from.
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

-- v2 (B0): function_dependencies -- call/type/cast/operator edges
-- between shim functions. Populated by the source walker
-- (call/call_method/macro/indirect) and by SQL-derived queries
-- (type_arg/type_return/cast_target/cast_source/operator_bind).
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

-- v2 (B0): test_cases -- one row per stable test-case identity
-- for a function. Populated by B1 test importers.
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

-- v2 (B0): test_runs -- one row per test-case execution. Populated
-- by the B2 verification harness; extractor never writes here.
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

-- v2 (B0): status enum guards. SQLite lacks native ENUM but a
-- BEFORE INSERT trigger enforcing the discriminant works for both
-- extractor writes and hand-edits.
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

-- v4 (B4): status enum guards for the five newly-tracked
-- catalog tables. Same discriminant as the function-side triggers.
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

CREATE VIEW IF NOT EXISTS extension_counts AS
SELECT
    e.name AS extension,
    e.version,
    (SELECT COUNT(*) FROM scalars WHERE extension = e.name) AS scalars,
    (SELECT COUNT(*) FROM aggregates WHERE extension = e.name) AS aggregates,
    (SELECT COUNT(*) FROM table_functions WHERE extension = e.name) AS table_functions,
    (SELECT COUNT(*) FROM window_functions WHERE extension = e.name) AS window_functions,
    (SELECT COUNT(*) FROM column_types WHERE extension = e.name) AS column_types,
    (SELECT COUNT(*) FROM operators WHERE extension = e.name) AS operators,
    (SELECT COUNT(*) FROM cast_rewrites WHERE extension = e.name) AS cast_rewrites,
    (SELECT COUNT(*) FROM preprocessor_patterns WHERE extension = e.name) AS preprocessor_patterns,
    (SELECT COUNT(*) FROM system_catalog_tables WHERE extension = e.name) AS system_catalog_tables,
    (SELECT COUNT(*) FROM spatial_indexes WHERE extension = e.name) AS spatial_indexes
FROM extensions e;

-- v2 (B0): rolled-up function-status counts, split by extension and
-- function kind. Consumed by the coverage dashboards and by CI to
-- flag drift between counts and hash-tracked verification status.
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

-- v2 (B0): "if this function/type/operator changes, which SQL
-- functions could ripple?" -- WITH RECURSIVE reverse closure over
-- function_dependencies, cycle-guarded at depth 16 (empirically 4x
-- the max chain length observed in the postgis shim's Self:: cycles).
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

-- v3 (B3), extended v4 (B4): status roll-up rolled per
-- (extension, status) across every tracked catalog table.
-- Consumed by funcs-md-gen and the coverage dashboards. B4
-- widens the union from four function tables to nine so the
-- Types/Operators/Casts/Spatial Indexes/Preprocessor Patterns
-- lineage columns are visible in the same view.
CREATE VIEW IF NOT EXISTS status_summary_per_extension AS
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

-- v4 (B4): status roll-up split by (extension, kind, status) —
-- keeps the kind axis so funcs-md-gen can render one line per
-- entity kind ("Types (29): 0 verified / 29 unverified", ...).
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

-- v3 (B3), extended v4 (B4): per-leaf coverage. `leaf` comes from
-- the first entry of `test_cases.tags_json` that carries the
-- `leaf:*` prefix (scraper convention: leaf tag is the second
-- element, after the corpus tag). We surface leaves via json_each
-- lookup, falling back to `$[0]`.
--
-- v4 broadens the "verified?" check from scalars-only to a
-- COALESCE over every table where `test_cases.function_name`
-- can match a row identity (scalars.name / aggregates.name /
-- table_functions.name / window_functions.name). Types /
-- operators / casts / spatial indexes / preprocessor patterns
-- don't get verified via `test_cases.function_name` today, so
-- they aren't joined here — they surface in
-- `status_summary_per_kind` instead. Adding them as extra LEFT
-- JOINs would risk row-multiplication on the aggregate side.
CREATE VIEW IF NOT EXISTS leaf_coverage AS
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

-- v3 (B3): last-verified freshness — one row per scalar with a
-- non-null `implemented_verified` status, so CI can spot rows whose
-- last verification predates the shim's current upstream version.
CREATE VIEW IF NOT EXISTS verification_freshness AS
    SELECT extension, name, status, last_verified_at,
           last_verified_upstream_version
    FROM scalars
    WHERE status = 'implemented_verified';

PRAGMA user_version = 4;
