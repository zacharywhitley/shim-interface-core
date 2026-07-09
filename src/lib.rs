//! Extract a DataFission wasm shim's SQL surface into a SQLite
//! database.
//!
//! The shim is any composed `.wasm` produced via `wac plug`
//! against `datafission:df-plugin-api/extension@1.0.0`. This
//! crate's job is to walk the shim's registry --
//! [`RuntimeWasmExtension::register`] plus
//! [`RuntimeWasmExtension::extract_sql_metadata`] -- and write
//! every scalar / aggregate / table function / window function /
//! column type / system catalog / spatial index / cast / operator /
//! preprocessor pattern it advertises into a SQLite database.
//!
//! B0 (2026-07-08) extends the schema with per-function lineage
//! tracking: `interface`, `signature_hash`,
//! `implementation_hash`, upstream-version columns, plus new
//! `upstream_versions` / `function_dependencies` /
//! `test_cases` / `test_runs` tables. See [`version`] for the
//! migration discipline and [`hashes`] for the hash formulae.
//!
//! Output is a portable artifact; downstream consumers
//! (sqlink, ducklink) read it without ever loading the wasm.

pub mod hashes;
pub mod migrations;
pub mod owner;
pub mod version;
pub mod walker;

pub use owner::{OwnerResolver, SourceMetadata, StaticOwnerResolver};
pub use version::{ensure_schema, read_user_version, SCHEMA_VERSION};

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection};

use datafission_df_plugin_api::{
    DataTypePlugin, Extension as ExtensionTrait, ExtensionError, ExtensionTarget,
    SystemCatalogProvider, SystemTable,
};
use datafission_df_plugin_loader::{
    extract_postgis_metadata_from_plug, ExtractedCast, ExtractedOperator,
    ExtractedPreprocessor, PostgisMetadataSnapshot, RuntimeWasmExtension,
    SqlMetadataSnapshot,
};
use datafission_functions::traits::{
    AggregateFunctionDef, ScalarFunctionDef, TableFunctionDef, WindowFunctionDef,
};
use datafission_index::traits::IndexBuilder;

/// The embedded schema. Apply once after opening a Connection.
pub const SCHEMA_SQL: &str = include_str!("schema.sql");

/// A handle the caller shares across extract_shim calls. We need
/// the indirection because the `ExtensionTarget` trait requires
/// the impl to be `'static` (so it can be downcast via
/// `as_any_mut`), which rules out a borrowed `&Connection`.
pub type SharedConn = Rc<RefCell<Connection>>;

/// Open a database, replace any existing file, apply the schema,
/// and return a shareable handle. `SCHEMA_SQL` writes
/// `PRAGMA user_version = 3` at its tail, so no explicit tagging
/// is needed here.
pub fn open_fresh(path: &Path) -> Result<SharedConn> {
    if path != Path::new(":memory:") && path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("removing prior {}", path.display()))?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(SCHEMA_SQL)?;
    // Defensive: schema.sql ends with `PRAGMA user_version = 3` but a
    // future refactor might drop it accidentally. `ensure_schema`
    // treats an on-target DB as a no-op.
    ensure_schema(&conn)?;
    Ok(Rc::new(RefCell::new(conn)))
}

/// Non-destructive open: preserve any existing content, run the
/// forward-only migration if the DB is older than
/// [`SCHEMA_VERSION`]. Used by the backfill script and any
/// long-lived tool that keeps a shim-interface DB around across
/// extractions.
pub fn open_or_migrate(path: &Path) -> Result<SharedConn> {
    let conn = Connection::open(path)?;
    // If the DB is empty (first-time open on a non-existent file),
    // execute the schema to bring it directly to the current version.
    let has_tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'extensions'",
        [],
        |r| r.get(0),
    )?;
    if has_tables == 0 {
        conn.execute_batch(SCHEMA_SQL)?;
    } else {
        ensure_schema(&conn)?;
    }
    Ok(Rc::new(RefCell::new(conn)))
}

/// Extract one shim into the shared connection. Legacy shape --
/// no source-metadata pass. See
/// [`extract_shim_with_source`] for the B0 lineage flow.
pub fn extract_shim(conn: &SharedConn, wasm_path: &Path) -> Result<ExtractedSummary> {
    extract_shim_inner(conn, wasm_path, None)
}

/// Extract one shim and, when `source` is provided, hash every
/// function row, populate `first_seen_upstream_version` /
/// `last_seen_upstream_version`, walk the Rust source for
/// call/method/macro/indirect edges, derive type/cast/operator
/// edges from the catalog columns, and stamp an
/// `upstream_versions` row.
pub fn extract_shim_with_source(
    conn: &SharedConn,
    wasm_path: &Path,
    source: SourceMetadata<'_>,
) -> Result<ExtractedSummary> {
    extract_shim_inner(conn, wasm_path, Some(source))
}

fn extract_shim_inner(
    conn: &SharedConn,
    wasm_path: &Path,
    source: Option<SourceMetadata<'_>>,
) -> Result<ExtractedSummary> {
    let abs = wasm_path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", wasm_path.display()))?;
    let blake3 = blake3_of_file(&abs)?;

    let ext = RuntimeWasmExtension::from_file(&abs)
        .map_err(|e| anyhow!("loading {}: {e}", abs.display()))?;
    let name = ext.name().to_string();
    let version = ext.version().to_string();

    conn.borrow().execute(
        "INSERT INTO extensions \
         (name, version, api_version, wasm_path, wasm_blake3, extracted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            name,
            version,
            version,
            abs.display().to_string(),
            blake3,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;

    let mut target = SqliteExtensionTarget {
        conn: Rc::clone(conn),
        extension: name.clone(),
    };
    ext.register(&mut target)
        .map_err(|e| anyhow!("register({name}): {e}"))?;

    let SqlMetadataSnapshot {
        casts,
        operators,
        preprocessors,
    } = ext
        .extract_sql_metadata()
        .map_err(|e| anyhow!("extract_sql_metadata({name}): {e}"))?;
    let c = conn.borrow();
    insert_casts(&c, &name, &casts)?;
    insert_operators(&c, &name, &operators)?;
    insert_preprocessors(&c, &name, &preprocessors)?;
    drop(c);

    // Drain the spatial-index-plugin/spatial-index@1.0.0 interface
    // — PostGIS publishes its spatial-index here (process-global)
    // rather than through the per-extension index-plugin callback
    // that ExtensionTarget::register_index_builder consumes. Without
    // this drain, PostGIS extractions reported `spatial_indexes = 0`
    // even though the shim has a working spatial index.
    if let Ok(Some(meta)) = ext.extract_spatial_index_metadata() {
        let caps = serde_json::json!({
            "knn": meta.capabilities.knn,
            "within_distance": meta.capabilities.within_distance,
            "within_distance_wkb": meta.capabilities.within_distance_wkb,
            "update_after_build": meta.capabilities.update_after_build,
        }).to_string();
        let c = conn.borrow();
        // One row per alias the shim publishes; type_id is 0
        // because the spatial-index interface routes by alias,
        // not by stable id.
        for alias in &meta.aliases {
            let _ = c.execute(
                "INSERT OR IGNORE INTO spatial_indexes \
                 (extension, name, type_id, capabilities_json) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![name, alias, 0i64, caps],
            );
        }
        let _ = (meta.name,);  // index "registry name" (e.g. "postgis-rtree")
                               // captured in capabilities_json discussion; the
                               // alias column is what callers query by.
    }

    if let Some(src) = source {
        extract_source_metadata(conn, &name, &src)
            .with_context(|| format!("extract_source_metadata({name})"))?;
    }

    Ok(ExtractedSummary { name, version, blake3 })
}

/// Post-registration pass that hashes every function row, fills
/// interface / first_seen / last_seen columns, walks the Rust
/// source for call edges, derives type/cast/operator edges, and
/// records an `upstream_versions` row. See `SourceMetadata`.
///
/// `interface` is resolved for each row via the
/// [`OwnerResolver::owner_file`] map: rows for which the resolver
/// has no mapping keep `interface = NULL`, but their signature
/// hash still lands (implementation hash only folds the helpers
/// tree when the owner is unmapped).
pub fn extract_source_metadata(
    conn: &SharedConn,
    extension_name: &str,
    src: &SourceMetadata<'_>,
) -> Result<()> {
    let helpers = hashes::helpers_hash(src.helpers_root)?;

    // Pre-resolve owner file per interface, and its
    // implementation hash (cached across every row that shares
    // the same owner).
    let interfaces = src.owner_map.known_interfaces();
    let mut impl_hash_by_iface: std::collections::HashMap<String, (Option<std::path::PathBuf>, String)> =
        std::collections::HashMap::new();
    for iface in &interfaces {
        let owner = src.owner_map.owner_file(iface);
        let hash = hashes::implementation_hash(owner.as_deref(), &helpers)?;
        impl_hash_by_iface.insert(iface.clone(), (owner, hash));
    }
    // A row with no interface still needs an implementation hash
    // (owner=None, helpers-only). Precompute once.
    let helpers_only_impl_hash = hashes::implementation_hash(None, &helpers)?;

    // 1. Signature+implementation hash writes, upstream-version
    //    stamping, interface backfill via the file-stem heuristic
    //    (owner->file), for every function row.
    let c = conn.borrow();
    stamp_scalar_rows(&c, extension_name, src, &impl_hash_by_iface, &helpers_only_impl_hash)?;
    stamp_aggregate_rows(&c, extension_name, src, &impl_hash_by_iface, &helpers_only_impl_hash)?;
    stamp_simple_fn_rows(&c, "table_functions", extension_name, src, &impl_hash_by_iface, &helpers_only_impl_hash)?;
    stamp_simple_fn_rows(&c, "window_functions", extension_name, src, &impl_hash_by_iface, &helpers_only_impl_hash)?;
    drop(c);

    // 2. Walker-derived edges from the Rust source tree.
    if !src.skip_source_walk {
        let walked = walker::walk_shim_src(src.src_root)?;
        let c = conn.borrow();
        insert_call_edges(&c, extension_name, &walked)?;
        drop(c);
    }

    // 3. SQL-derived edges from catalog columns.
    let c = conn.borrow();
    insert_derived_edges(&c, extension_name)?;
    drop(c);

    // 4. Upstream-versions row.
    let c = conn.borrow();
    let scalar_count: i64 = c.query_row(
        "SELECT COUNT(*) FROM scalars WHERE extension = ?1",
        params![extension_name],
        |r| r.get(0),
    )?;
    let aggregate_count: i64 = c.query_row(
        "SELECT COUNT(*) FROM aggregates WHERE extension = ?1",
        params![extension_name],
        |r| r.get(0),
    )?;
    let table_fn_count: i64 = c.query_row(
        "SELECT COUNT(*) FROM table_functions WHERE extension = ?1",
        params![extension_name],
        |r| r.get(0),
    )?;
    let window_fn_count: i64 = c.query_row(
        "SELECT COUNT(*) FROM window_functions WHERE extension = ?1",
        params![extension_name],
        |r| r.get(0),
    )?;
    c.execute(
        "INSERT OR REPLACE INTO upstream_versions \
         (extension, version, released_at, ingested_at, ingested_from_commit, \
          scalar_count, aggregate_count, table_function_count, window_function_count, notes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
        params![
            extension_name,
            src.upstream_version,
            src.released_at,
            chrono::Utc::now().to_rfc3339(),
            src.upstream_commit,
            scalar_count,
            aggregate_count,
            table_fn_count,
            window_fn_count,
        ],
    )?;
    Ok(())
}

fn interface_lookup<'a>(
    impl_map: &'a std::collections::HashMap<String, (Option<std::path::PathBuf>, String)>,
    row_name: &str,
) -> Option<&'a str> {
    // First: fall through -- we don't know which interface owns
    // `row_name` from the row alone. Owner discovery is
    // per-interface, not per-function. See `SourceMetadata`
    // doc-comment.
    let _ = (impl_map, row_name);
    None
}

fn stamp_scalar_rows(
    c: &Connection,
    extension: &str,
    src: &SourceMetadata<'_>,
    impl_hash_by_iface: &std::collections::HashMap<String, (Option<std::path::PathBuf>, String)>,
    helpers_only_impl_hash: &str,
) -> Result<()> {
    // The interface column is set from the row's owner-file
    // heuristic. Because the plugin loader currently doesn't
    // surface `interface` per-registration (B0 note: interface
    // tracking through `enter_interface` on ExtensionTarget lands
    // in a follow-up), we leave `interface` as-is when it's
    // already set and otherwise attempt a same-name lookup: if
    // exactly one owner mapping exists for the row, adopt it.
    // Otherwise the column stays NULL and the implementation
    // hash reduces to the helpers-only fold.
    let mut sel = c.prepare(
        "SELECT name, param_types_json, return_type, is_deterministic, propagates_null, \
                interface, first_seen_upstream_version \
         FROM scalars WHERE extension = ?1",
    )?;
    let rows: Vec<(String, String, String, i64, i64, Option<String>, Option<String>)> = sel
        .query_map(params![extension], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut upd = c.prepare(
        "UPDATE scalars \
            SET interface = COALESCE(interface, ?3), \
                first_seen_upstream_version = COALESCE(first_seen_upstream_version, ?4), \
                last_seen_upstream_version = ?4, \
                signature_hash = ?5, \
                implementation_hash = ?6 \
          WHERE extension = ?1 AND name = ?2",
    )?;
    for (name, pt, rt, is_det, prop_null, iface, _first_seen) in rows {
        let sig = hashes::scalar_signature_hash(&hashes::ScalarSig {
            name: &name,
            param_types_json: &pt,
            return_type: &rt,
            is_deterministic: is_det != 0,
            propagates_null: prop_null != 0,
        });
        let (iface_for_write, impl_hash) = resolve_iface_and_impl(
            iface.as_deref(),
            interface_lookup(impl_hash_by_iface, &name),
            impl_hash_by_iface,
            helpers_only_impl_hash,
        );
        upd.execute(params![
            extension,
            name,
            iface_for_write,
            src.upstream_version,
            sig,
            impl_hash,
        ])?;
    }
    Ok(())
}

fn stamp_aggregate_rows(
    c: &Connection,
    extension: &str,
    src: &SourceMetadata<'_>,
    impl_hash_by_iface: &std::collections::HashMap<String, (Option<std::path::PathBuf>, String)>,
    helpers_only_impl_hash: &str,
) -> Result<()> {
    let mut sel = c.prepare(
        "SELECT name, param_types_json, supports_grouped, supports_partial, \
                is_order_sensitive, accepts_config, config_arg_indices_json, interface \
         FROM aggregates WHERE extension = ?1",
    )?;
    let rows: Vec<(String, String, i64, i64, i64, i64, String, Option<String>)> = sel
        .query_map(params![extension], |r| {
            Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut upd = c.prepare(
        "UPDATE aggregates \
            SET interface = COALESCE(interface, ?3), \
                first_seen_upstream_version = COALESCE(first_seen_upstream_version, ?4), \
                last_seen_upstream_version = ?4, \
                signature_hash = ?5, \
                implementation_hash = ?6 \
          WHERE extension = ?1 AND name = ?2",
    )?;
    for (name, pt, sg, sp, os, ac, cfg_json, iface) in rows {
        let sig = hashes::aggregate_signature_hash(&hashes::AggregateSig {
            name: &name,
            param_types_json: &pt,
            supports_grouped: sg != 0,
            supports_partial: sp != 0,
            is_order_sensitive: os != 0,
            accepts_config: ac != 0,
            config_arg_indices_json: &cfg_json,
        });
        let (iface_for_write, impl_hash) = resolve_iface_and_impl(
            iface.as_deref(),
            interface_lookup(impl_hash_by_iface, &name),
            impl_hash_by_iface,
            helpers_only_impl_hash,
        );
        upd.execute(params![
            extension,
            name,
            iface_for_write,
            src.upstream_version,
            sig,
            impl_hash,
        ])?;
    }
    Ok(())
}

fn stamp_simple_fn_rows(
    c: &Connection,
    table: &str,
    extension: &str,
    src: &SourceMetadata<'_>,
    impl_hash_by_iface: &std::collections::HashMap<String, (Option<std::path::PathBuf>, String)>,
    helpers_only_impl_hash: &str,
) -> Result<()> {
    let sel_sql = format!(
        "SELECT name, param_types_json, interface FROM {table} WHERE extension = ?1"
    );
    let mut sel = c.prepare(&sel_sql)?;
    let rows: Vec<(String, String, Option<String>)> = sel
        .query_map(params![extension], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let upd_sql = format!(
        "UPDATE {table} \
            SET interface = COALESCE(interface, ?3), \
                first_seen_upstream_version = COALESCE(first_seen_upstream_version, ?4), \
                last_seen_upstream_version = ?4, \
                signature_hash = ?5, \
                implementation_hash = ?6 \
          WHERE extension = ?1 AND name = ?2"
    );
    let mut upd = c.prepare(&upd_sql)?;
    for (name, pt, iface) in rows {
        let sig_arg = hashes::SimpleFnSig {
            name: &name,
            param_types_json: &pt,
        };
        let sig = if table == "table_functions" {
            hashes::table_function_signature_hash(&sig_arg)
        } else {
            hashes::window_function_signature_hash(&sig_arg)
        };
        let (iface_for_write, impl_hash) = resolve_iface_and_impl(
            iface.as_deref(),
            interface_lookup(impl_hash_by_iface, &name),
            impl_hash_by_iface,
            helpers_only_impl_hash,
        );
        upd.execute(params![
            extension,
            name,
            iface_for_write,
            src.upstream_version,
            sig,
            impl_hash,
        ])?;
    }
    Ok(())
}

fn resolve_iface_and_impl(
    row_iface: Option<&str>,
    guessed: Option<&str>,
    impl_hash_by_iface: &std::collections::HashMap<String, (Option<std::path::PathBuf>, String)>,
    helpers_only_impl_hash: &str,
) -> (Option<String>, String) {
    let iface = row_iface.or(guessed).map(|s| s.to_string());
    let impl_hash = iface
        .as_ref()
        .and_then(|i| impl_hash_by_iface.get(i).map(|(_, h)| h.clone()))
        .unwrap_or_else(|| helpers_only_impl_hash.to_string());
    (iface, impl_hash)
}

fn insert_call_edges(
    c: &Connection,
    extension: &str,
    walked: &[walker::WalkedFn],
) -> Result<()> {
    // Filter: only record edges whose caller is a known
    // scalar/aggregate/table/window row -- otherwise every
    // internal helper collision produces noise.
    fn load_names(
        c: &Connection,
        sql: &str,
        extension: &str,
    ) -> Result<std::collections::HashSet<String>> {
        let mut s = c.prepare(sql)?;
        let iter = s.query_map(params![extension], |r| r.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for row in iter {
            out.insert(row?);
        }
        Ok(out)
    }
    let known_scalars = load_names(c, "SELECT name FROM scalars WHERE extension = ?1", extension)?;
    let known_aggregates =
        load_names(c, "SELECT name FROM aggregates WHERE extension = ?1", extension)?;
    let known_table_fns =
        load_names(c, "SELECT name FROM table_functions WHERE extension = ?1", extension)?;
    let known_window_fns =
        load_names(c, "SELECT name FROM window_functions WHERE extension = ?1", extension)?;

    let mut ins = c.prepare(
        "INSERT OR IGNORE INTO function_dependencies \
         (extension, caller_name, caller_kind, callee_extension, callee_name, callee_kind, edge_kind, source_hint) \
         VALUES (?1, ?2, ?3, ?1, ?4, ?5, ?6, ?7)",
    )?;
    for w in walked {
        let caller_kind = if known_scalars.contains(&w.caller_name) {
            "scalar"
        } else if known_aggregates.contains(&w.caller_name) {
            "aggregate"
        } else if known_table_fns.contains(&w.caller_name) {
            "table"
        } else if known_window_fns.contains(&w.caller_name) {
            "window"
        } else {
            continue;
        };
        for e in &w.edges {
            let callee_kind = if known_scalars.contains(&e.callee_name) {
                "scalar"
            } else if known_aggregates.contains(&e.callee_name) {
                "aggregate"
            } else if known_table_fns.contains(&e.callee_name) {
                "table"
            } else if known_window_fns.contains(&e.callee_name) {
                "window"
            } else {
                match e.edge_kind {
                    walker::EdgeKind::Macro => "macro",
                    walker::EdgeKind::CallMethod => "method",
                    walker::EdgeKind::Indirect => "indirect",
                    walker::EdgeKind::Call => "indirect",
                }
            };
            ins.execute(params![
                extension,
                w.caller_name,
                caller_kind,
                e.callee_name,
                callee_kind,
                e.edge_kind.as_str(),
                e.source_hint,
            ])?;
        }
    }
    Ok(())
}

fn insert_derived_edges(c: &Connection, extension: &str) -> Result<()> {
    // (a) type_arg + type_return -- walk param_types_json (JSON
    // array of arrays of type-name strings) and cross-reference
    // column_types. See B0 §4.4.
    for (kind_col, kind_val) in [
        ("scalars", "scalar"),
        ("aggregates", "aggregate"),
        ("table_functions", "table"),
        ("window_functions", "window"),
    ] {
        let sql = format!(
            "INSERT OR IGNORE INTO function_dependencies \
             (extension, caller_name, caller_kind, callee_extension, callee_name, \
              callee_kind, edge_kind, source_hint) \
             SELECT s.extension, s.name, ?2, s.extension, ct.type_name, 'type', 'type_arg', \
                    'arg_index=' || jouter.key \
             FROM {kind_col} s \
             JOIN json_each(s.param_types_json) AS jouter ON 1=1 \
             JOIN json_each(jouter.value)       AS j      ON 1=1 \
             JOIN column_types ct ON ct.extension = s.extension AND ct.type_name = j.value \
             WHERE j.type = 'text' AND s.extension = ?1"
        );
        c.execute(&sql, params![extension, kind_val])?;
    }

    // return_type edge (scalars only -- other kinds don't have a
    // scalar return_type column).
    c.execute(
        "INSERT OR IGNORE INTO function_dependencies \
         (extension, caller_name, caller_kind, callee_extension, callee_name, callee_kind, edge_kind, source_hint) \
         SELECT s.extension, s.name, 'scalar', s.extension, ct.type_name, 'type', 'type_return', 'return' \
         FROM scalars s \
         JOIN column_types ct ON ct.extension = s.extension AND ct.type_name = s.return_type \
         WHERE s.extension = ?1",
        params![extension],
    )?;

    // (b) cast_target edges.
    c.execute(
        "INSERT OR IGNORE INTO function_dependencies \
         (extension, caller_name, caller_kind, callee_extension, callee_name, callee_kind, edge_kind, source_hint) \
         SELECT cr.extension, cr.function_name, 'scalar', cr.extension, ct.type_name, 'type', 'cast_target', \
                'cast_from=' || cr.source_kind || '/' || cr.source_fn_hint \
         FROM cast_rewrites cr \
         JOIN column_types ct ON ct.extension = cr.extension AND ct.type_name = cr.target_type \
         WHERE cr.extension = ?1",
        params![extension],
    )?;

    // (c) operator_bind edges.
    c.execute(
        "INSERT OR IGNORE INTO function_dependencies \
         (extension, caller_name, caller_kind, callee_extension, callee_name, callee_kind, edge_kind, source_hint) \
         SELECT o.extension, o.function_name, 'scalar', o.extension, o.symbol, 'operator', 'operator_bind', \
                'lhs=' || IFNULL(o.lhs_type_id,'') || ' rhs=' || IFNULL(o.rhs_type_id,'') \
         FROM operators o \
         WHERE o.extension = ?1",
        params![extension],
    )?;
    Ok(())
}

/// Drain the raw `postgis-composed.wasm` plug's
/// `postgis:wasm/postgis-metadata@0.1.0` surface (#784) into the
/// same `cast_rewrites` / `operators` / `preprocessor_patterns`
/// tables that `extract_shim` populates from
/// `datafission:sql-extension-plugin/metadata`.
///
/// PostGIS #788 — the datafission postgis bridge implements
/// `sql-extension-plugin/metadata` by returning `Ok(Vec::new())`
/// for all three lists (its custom-type registration path is
/// where PostGIS' rewrite tables would normally get seeded).
/// The `postgis:wasm/postgis-metadata` surface on the raw plug
/// carries the real 39/43/5 cast/operator/preprocessor tables,
/// but `wac plug` hides plug exports so the composed shim can't
/// re-expose them. Extractors therefore need to drain from the
/// plug wasm alongside the shim wasm; that's what this function
/// does.
///
/// Rows are attributed to `extension_name` (typically `"postgis"`)
/// so downstream `cast_rewrites` queries by extension name still
/// work identically. `plug_wasm_path` is the raw plug, e.g.
/// `<datafission>/extensions/postgis/deps/postgis-composed.wasm`.
///
/// Returns `Ok(None)` when the plug wasm doesn't export
/// `postgis:wasm/postgis-metadata@0.1.0` (older pins predating
/// #784). Callers should treat that as "no metadata to drain".
pub fn drain_postgis_metadata(
    conn: &SharedConn,
    extension_name: &str,
    plug_wasm_path: &Path,
) -> Result<Option<PostgisMetadataCounts>> {
    let abs = plug_wasm_path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", plug_wasm_path.display()))?;
    let snap = extract_postgis_metadata_from_plug(&abs)
        .map_err(|e| anyhow!("extract_postgis_metadata_from_plug({}): {e}", abs.display()))?;
    let Some(PostgisMetadataSnapshot { casts, operators, preprocessors }) = snap else {
        return Ok(None);
    };
    let counts = PostgisMetadataCounts {
        casts: casts.len(),
        operators: operators.len(),
        preprocessors: preprocessors.len(),
    };
    let c = conn.borrow();
    insert_casts(&c, extension_name, &casts)?;
    insert_operators(&c, extension_name, &operators)?;
    insert_preprocessors(&c, extension_name, &preprocessors)?;
    drop(c);
    Ok(Some(counts))
}

/// Per-list row counts returned by [`drain_postgis_metadata`],
/// intended for the per-shim binary's `--summary` output so a
/// human running the extraction can spot-check that #784's
/// 39/43/5 tables actually landed.
#[derive(Debug, Clone, Copy)]
pub struct PostgisMetadataCounts {
    pub casts: usize,
    pub operators: usize,
    pub preprocessors: usize,
}

/// Print a per-extension count summary to stdout. Useful for
/// the trailing line of `--summary` flag in the per-shim binaries.
pub fn print_summary(conn: &SharedConn) -> Result<()> {
    let c = conn.borrow();
    let mut stmt = c.prepare(
        "SELECT extension, version, scalars, aggregates, table_functions, \
                window_functions, column_types, operators, cast_rewrites, \
                preprocessor_patterns, system_catalog_tables, spatial_indexes \
         FROM extension_counts ORDER BY extension",
    )?;
    let mut rows = stmt.query([])?;
    println!();
    println!(
        "{:14} {:8} {:>8} {:>5} {:>5} {:>7} {:>6} {:>6} {:>6} {:>6} {:>7} {:>7}",
        "extension", "version", "scalars", "agg", "udtf", "window", "types", "ops",
        "casts", "preps", "catalog", "idx"
    );
    while let Some(r) = rows.next()? {
        println!(
            "{:14} {:8} {:>8} {:>5} {:>5} {:>7} {:>6} {:>6} {:>6} {:>6} {:>7} {:>7}",
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, i64>(9)?,
            r.get::<_, i64>(10)?,
            r.get::<_, i64>(11)?,
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ExtractedSummary {
    pub name: String,
    pub version: String,
    pub blake3: String,
}

fn blake3_of_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

// ---------------------------------------------------------------------------
// SqliteExtensionTarget — captures every register_* call into SQLite rows.
// ---------------------------------------------------------------------------

struct SqliteExtensionTarget {
    conn: SharedConn,
    extension: String,
}

impl ExtensionTarget for SqliteExtensionTarget {
    fn register_scalar_function(
        &mut self,
        _namespace: &str,
        def: Arc<dyn ScalarFunctionDef>,
    ) -> Result<(), ExtensionError> {
        let pt = serde_json::to_string(
            &def.param_types()
                .iter()
                .map(|sig| sig.iter().map(|t| format!("{:?}", t).to_lowercase()).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into());
        let first = def.param_types().first().cloned().unwrap_or_default();
        let rt = format!("{:?}", def.return_type(&first)).to_lowercase();
        let name = def.name().to_string();
        let _ = self.conn.borrow().execute(
            "INSERT OR IGNORE INTO scalars \
             (extension, name, param_types_json, return_type, is_deterministic, propagates_null) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                self.extension, name, pt, rt,
                def.is_deterministic() as i32, def.propagates_null() as i32
            ],
        );
        for alias in def.aliases() {
            let _ = self.conn.borrow().execute(
                "INSERT OR IGNORE INTO scalar_aliases (extension, canonical, alias) \
                 VALUES (?1, ?2, ?3)",
                params![self.extension, name, alias],
            );
        }
        Ok(())
    }

    fn register_aggregate_function(
        &mut self,
        _namespace: &str,
        def: Arc<dyn AggregateFunctionDef>,
    ) -> Result<(), ExtensionError> {
        let pt = serde_json::to_string(
            &def.param_types()
                .iter()
                .map(|sig| sig.iter().map(|t| format!("{:?}", t).to_lowercase()).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into());
        let cfg_idx = serde_json::to_string(&def.config_arg_indices())
            .unwrap_or_else(|_| "[]".into());
        let name = def.name().to_string();
        // Skip if the same SQL name was already registered as a window
        // function. Some upstream shims (e.g. PostGIS' st_clusterdbscan)
        // declare a function under both the aggregate and window slots,
        // but only the window shape has a matching WIT export. Letting
        // the aggregate row through produces spurious "no WIT aggregate"
        // warnings in codegen. Window wins.
        {
            let c = self.conn.borrow();
            let already_window: bool = c
                .query_row(
                    "SELECT 1 FROM window_functions WHERE extension = ?1 AND name = ?2",
                    params![self.extension, name],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if already_window {
                return Ok(());
            }
        }
        let _ = self.conn.borrow().execute(
            "INSERT OR IGNORE INTO aggregates \
             (extension, name, param_types_json, supports_grouped, supports_partial, \
              is_order_sensitive, accepts_config, config_arg_indices_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                self.extension, name, pt,
                1i32, 1i32, 0i32,
                def.accepts_config() as i32,
                cfg_idx,
            ],
        );
        for alias in def.aliases() {
            let _ = self.conn.borrow().execute(
                "INSERT OR IGNORE INTO aggregate_aliases (extension, canonical, alias) \
                 VALUES (?1, ?2, ?3)",
                params![self.extension, name, alias],
            );
        }
        Ok(())
    }

    fn register_table_function(
        &mut self,
        _namespace: &str,
        def: Arc<dyn TableFunctionDef>,
    ) -> Result<(), ExtensionError> {
        let pt = serde_json::to_string(
            &def.param_types()
                .iter()
                .map(|sig| sig.iter().map(|t| format!("{:?}", t).to_lowercase()).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into());
        let name = def.name().to_string();
        let _ = self.conn.borrow().execute(
            "INSERT OR IGNORE INTO table_functions (extension, name, param_types_json) \
             VALUES (?1, ?2, ?3)",
            params![self.extension, name, pt],
        );
        for alias in def.aliases() {
            let _ = self.conn.borrow().execute(
                "INSERT OR IGNORE INTO table_function_aliases (extension, canonical, alias) \
                 VALUES (?1, ?2, ?3)",
                params![self.extension, name, alias],
            );
        }
        Ok(())
    }

    fn register_window_function(
        &mut self,
        _namespace: &str,
        def: Arc<dyn WindowFunctionDef>,
    ) -> Result<(), ExtensionError> {
        let pt = serde_json::to_string(
            &def.param_types()
                .iter()
                .map(|sig| sig.iter().map(|t| format!("{:?}", t).to_lowercase()).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into());
        let name = def.name().to_string();
        let _ = self.conn.borrow().execute(
            "INSERT OR IGNORE INTO window_functions (extension, name, param_types_json) \
             VALUES (?1, ?2, ?3)",
            params![self.extension, name, pt],
        );
        for alias in def.aliases() {
            let _ = self.conn.borrow().execute(
                "INSERT OR IGNORE INTO window_function_aliases (extension, canonical, alias) \
                 VALUES (?1, ?2, ?3)",
                params![self.extension, name, alias],
            );
        }
        // If the same SQL name was previously registered as an aggregate
        // (registration order is shim-defined), drop the aggregate row so
        // codegen sees a single canonical kind for this function. See
        // register_aggregate_function for the inverse-order case.
        let _ = self.conn.borrow().execute(
            "DELETE FROM aggregates WHERE extension = ?1 AND name = ?2",
            params![self.extension, name],
        );
        let _ = self.conn.borrow().execute(
            "DELETE FROM aggregate_aliases WHERE extension = ?1 AND canonical = ?2",
            params![self.extension, name],
        );
        Ok(())
    }

    fn register_data_type(
        &mut self,
        plugin: Arc<dyn DataTypePlugin>,
    ) -> Result<(), ExtensionError> {
        let _ = self.conn.borrow().execute(
            "INSERT OR IGNORE INTO column_types \
             (extension, type_id, type_name, storage_size, cast_from_json, cast_to_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                self.extension,
                plugin.type_id() as i64,
                plugin.type_name(),
                plugin.storage_size() as i64,
                "[]",
                "[]",
            ],
        );
        Ok(())
    }

    fn register_index_builder(
        &mut self,
        type_id: u32,
        _builder: Arc<dyn IndexBuilder>,
    ) -> Result<(), ExtensionError> {
        let _ = self.conn.borrow().execute(
            "INSERT OR IGNORE INTO spatial_indexes (extension, name, type_id) \
             VALUES (?1, ?2, ?3)",
            params![self.extension, format!("type_id={type_id}"), type_id as i64],
        );
        Ok(())
    }

    fn register_system_catalog_provider(
        &mut self,
        provider: Arc<dyn SystemCatalogProvider>,
    ) -> Result<(), ExtensionError> {
        let catalog = provider.catalog_name().to_string();
        for SystemTable { name, columns } in provider.list_tables() {
            let cols = serde_json::to_string(
                &columns
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "name": c.name,
                            "data_type": format!("{:?}", c.data_type).to_lowercase(),
                            "nullable": c.nullable,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_else(|_| "[]".into());
            let _ = self.conn.borrow().execute(
                "INSERT OR IGNORE INTO system_catalog_tables \
                 (extension, catalog_name, table_name, columns_json) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![self.extension, catalog, name, cols],
            );
        }
        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn insert_casts(conn: &Connection, extension: &str, casts: &[ExtractedCast]) -> Result<()> {
    for c in casts {
        // `source_type_id` maps `Option<u32>::None` to the sentinel
        // 0 so the PK column can stay NOT NULL (`NULL`-in-PK rows
        // would be treated as distinct by SQLite, which defeats the
        // dedup guarantee `INSERT OR IGNORE` gives us).
        let source_type_id = c.source_type_id.unwrap_or(0) as i64;
        conn.execute(
            "INSERT OR IGNORE INTO cast_rewrites \
             (extension, target_type, source_kind, function_name, source_fn_hint, source_type_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                extension,
                c.target_type,
                c.source_kind,
                c.function_name,
                c.source_fn_hint,
                source_type_id,
            ],
        )?;
    }
    Ok(())
}

fn insert_operators(
    conn: &Connection,
    extension: &str,
    ops: &[ExtractedOperator],
) -> Result<()> {
    for o in ops {
        conn.execute(
            "INSERT OR IGNORE INTO operators \
             (extension, symbol, lhs_type_id, rhs_type_id, function_name) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                extension, o.symbol,
                o.lhs_type_id.map(|n| n as i64),
                o.rhs_type_id.map(|n| n as i64),
                o.function_name,
            ],
        )?;
    }
    Ok(())
}

fn insert_preprocessors(
    conn: &Connection,
    extension: &str,
    pps: &[ExtractedPreprocessor],
) -> Result<()> {
    for p in pps {
        conn.execute(
            "INSERT OR IGNORE INTO preprocessor_patterns \
             (extension, op_token, function_name) \
             VALUES (?1, ?2, ?3)",
            params![extension, p.operator_token, p.function_name],
        )?;
    }
    Ok(())
}
