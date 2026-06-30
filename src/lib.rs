//! Extract a DataFission wasm shim's SQL surface into a SQLite
//! database.
//!
//! The shim is any composed `.wasm` produced via `wac plug`
//! against `datafission:df-plugin-api/extension@1.0.0`. This
//! crate's job is to walk the shim's registry —
//! [`RuntimeWasmExtension::register`] plus
//! [`RuntimeWasmExtension::extract_sql_metadata`] — and write
//! every scalar / aggregate / table function / window function /
//! column type / system catalog / spatial index / cast / operator /
//! preprocessor pattern it advertises into a SQLite database.
//!
//! Output is a portable artifact; downstream consumers
//! (sqlink, ducklink) read it without ever loading the wasm.

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
    ExtractedCast, ExtractedOperator, ExtractedPreprocessor, RuntimeWasmExtension,
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
/// and return a shareable handle.
pub fn open_fresh(path: &Path) -> Result<SharedConn> {
    if path != Path::new(":memory:") && path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("removing prior {}", path.display()))?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(Rc::new(RefCell::new(conn)))
}

/// Extract one shim into the shared connection.
pub fn extract_shim(conn: &SharedConn, wasm_path: &Path) -> Result<ExtractedSummary> {
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

    Ok(ExtractedSummary { name, version, blake3 })
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
        conn.execute(
            "INSERT OR IGNORE INTO cast_rewrites \
             (extension, target_type, source_kind, function_name, source_fn_hint) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                extension, c.target_type, c.source_kind, c.function_name, c.source_fn_hint
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
