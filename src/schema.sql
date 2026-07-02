-- Schema for the shim-interface SQLite database.
--
-- Every row's `extension` is the shim's WIT identity name
-- (`"postgis"` / `"mobilitydb"` etc.); composite keys are
-- `(extension, name)` so a single database can hold multiple
-- shims side-by-side. Snapshot diffs work by ATTACHing two
-- databases and comparing.

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
    PRIMARY KEY (extension, name)
);

-- Scalar function aliases.
--
-- Doctrine note (2026-06-23 investigation): scalar shims expose
-- aliases two ways. PostGIS uses `ScalarFunctionDef::aliases()`,
-- returning a non-empty Vec from one canonical impl — those
-- rows land here. MobilityDB instead pushes each alias as its
-- own canonical `(name, Kind)` dispatch-table entry; its
-- `aliases()` returns empty, so this table is empty for that
-- shim (all 1548 mobilitydb scalars sit in `scalars` only,
-- including its ~275 internal aliases). Both choices are
-- correct against the trait — the difference is bookkeeping
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
    PRIMARY KEY (extension, type_id)
);

CREATE TABLE IF NOT EXISTS operators (
    extension TEXT NOT NULL,
    symbol TEXT NOT NULL,
    lhs_type_id INTEGER,
    rhs_type_id INTEGER,
    function_name TEXT NOT NULL,
    PRIMARY KEY (extension, symbol, lhs_type_id, rhs_type_id)
);

CREATE TABLE IF NOT EXISTS cast_rewrites (
    extension TEXT NOT NULL,
    target_type TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    function_name TEXT NOT NULL,
    source_fn_hint TEXT NOT NULL,
    -- Extension-namespaced source-side type id (or 0 when the
    -- source shape is not discriminated by type — e.g. bytea-fed
    -- `st_geomfromwkb` accepts any bit pattern). Populated from the
    -- WIT-side `cast-rewrite.source-type-id` field (#798).
    source_type_id INTEGER NOT NULL DEFAULT 0,
    -- `source_fn_hint` and `source_type_id` are part of the PK so
    -- distinct source-side rewrites that share a (target_type,
    -- source_kind) key don't collide under `INSERT OR IGNORE`.
    -- PostGIS advertises many casts that discriminate by the
    -- source-side function (box2d::geometry vs. box3d::geometry,
    -- both under `any`) — those separate via `source_fn_hint`. It
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
--   1. `index-plugin/index@1.0.0` — per-extension callback,
--      surfaces through `ExtensionTarget::register_index_builder`.
--      `type_id` is the WIT-declared id; `capabilities_json` is
--      null because that interface doesn't carry capability flags.
--   2. `spatial-index-plugin/spatial-index@1.0.0` — process-
--      global, surfaces through `extract_spatial_index_metadata`.
--      `type_id` is 0 (sentinel — that interface doesn't expose
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
    PRIMARY KEY (extension, name)
);

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
