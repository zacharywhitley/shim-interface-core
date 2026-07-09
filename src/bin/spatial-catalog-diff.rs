//! spatial-catalog-diff — the layer-3 orchestrator's input.
//!
//! Compare two shim-interface catalogs and emit a per-family
//! added / removed / signature-changed / unchanged delta report.
//! Consumed by the cascade orchestrator (as JSON) and by humans
//! (as text).
//!
//! Both inputs may be either
//!   * a SQLite interface database written by
//!     [`shim_interface_core::open_fresh`] (v3 or v4), or
//!   * a `<extension>-catalog.toml` emitted by
//!     `sql-extension-catalog-emit`.
//!
//! Mixed inputs work too — the loader normalises both shapes to a
//! flat per-family entity list keyed by primary identity.  When a
//! side lacks `signature_hash` (all TOML inputs, and any v3 DB
//! table whose lineage columns pre-date B4) the loader marks the
//! side as `signatures_unavailable` and the diff falls back to
//! name-only add/remove reporting for that family.  The report
//! surfaces the fallback so downstream tooling knows it did not
//! see hash-level breakage.
//!
//! SEMVER classification (rough — advisory only, humans gate):
//!   * MAJOR when any family has removals.
//!   * MINOR when only additions and no breaking removals.
//!   * MINOR-WITH-CHANGES when there are signature changes but no
//!     removals — flagged for re-verification.
//!   * PATCH when nothing changed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "spatial-catalog-diff",
    about = "Diff two shim-interface catalogs (SQLite or TOML)."
)]
struct Cli {
    /// Path to the "before" catalog: a `.sqlite`/`.db`
    /// interface database or an emitted `-catalog.toml`.
    #[arg(long)]
    before: PathBuf,

    /// Path to the "after" catalog: same forms as `--before`.
    #[arg(long)]
    after: PathBuf,

    /// Emit `text` (human-friendly summary), `json` (structured
    /// delta for the cascade orchestrator), or `both` (single-shot
    /// dual emission — requires `--text-out` and `--json-out`).
    ///
    /// When omitted, the effective format is inferred from the
    /// `--text-out` / `--json-out` flags: both set → `both`, one
    /// set → the matching single format, neither set → `text`
    /// (streamed to stdout — the historical default).
    #[arg(long, value_enum)]
    output_format: Option<OutputFormat>,

    /// Write the human-friendly text report to this file instead
    /// of stdout. When set and `--output-format` is not, the
    /// effective format includes text.
    #[arg(long)]
    text_out: Option<PathBuf>,

    /// Write the structured JSON delta to this file instead of
    /// stdout. When set and `--output-format` is not, the
    /// effective format includes json.
    #[arg(long)]
    json_out: Option<PathBuf>,

    /// Comma-separated subset of families to report.  When empty
    /// (the default) every family is included.  Accepted names
    /// (aliases in parentheses):
    /// `scalars`, `aggregates`, `table_functions` (`tables`),
    /// `window_functions` (`windows`), `types` (`column_types`),
    /// `operators`, `casts` (`cast_rewrites`), `indexes`
    /// (`spatial_indexes`), `preprocessors`
    /// (`preprocessor_patterns`).
    #[arg(long, value_delimiter = ',')]
    entity_filter: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum OutputFormat {
    Text,
    Json,
    Both,
}

// ---------------------------------------------------------------------------
// Family taxonomy
// ---------------------------------------------------------------------------

/// The nine entity families the diff walks. Order here also
/// controls the order in which the text report prints the
/// sections.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[allow(clippy::enum_variant_names)]
enum Family {
    Scalars,
    Aggregates,
    TableFunctions,
    WindowFunctions,
    ColumnTypes,
    Operators,
    CastRewrites,
    SpatialIndexes,
    PreprocessorPatterns,
}

impl Family {
    const ALL: &'static [Family] = &[
        Family::Scalars,
        Family::Aggregates,
        Family::TableFunctions,
        Family::WindowFunctions,
        Family::ColumnTypes,
        Family::Operators,
        Family::CastRewrites,
        Family::SpatialIndexes,
        Family::PreprocessorPatterns,
    ];

    /// Canonical slug used in JSON output and as the key in
    /// per-family maps.
    fn slug(self) -> &'static str {
        match self {
            Family::Scalars => "scalars",
            Family::Aggregates => "aggregates",
            Family::TableFunctions => "table_functions",
            Family::WindowFunctions => "window_functions",
            Family::ColumnTypes => "column_types",
            Family::Operators => "operators",
            Family::CastRewrites => "cast_rewrites",
            Family::SpatialIndexes => "spatial_indexes",
            Family::PreprocessorPatterns => "preprocessor_patterns",
        }
    }

    /// Short label used in the text report header.
    fn label(self) -> &'static str {
        match self {
            Family::Scalars => "Scalars",
            Family::Aggregates => "Aggregates",
            Family::TableFunctions => "Table functions",
            Family::WindowFunctions => "Window functions",
            Family::ColumnTypes => "Types",
            Family::Operators => "Operators",
            Family::CastRewrites => "Casts",
            Family::SpatialIndexes => "Indexes",
            Family::PreprocessorPatterns => "Preprocessors",
        }
    }

    /// Only these families are guaranteed to carry
    /// `signature_hash` at v3 (B0). The five remaining families
    /// pick up hashes at v4 (B4).
    fn has_hash_at_v3(self) -> bool {
        matches!(
            self,
            Family::Scalars
                | Family::Aggregates
                | Family::TableFunctions
                | Family::WindowFunctions
        )
    }

    fn parse_slug(s: &str) -> Option<Family> {
        let norm = s.trim().to_ascii_lowercase();
        Some(match norm.as_str() {
            "scalars" | "scalar" => Family::Scalars,
            "aggregates" | "aggregate" | "agg" => Family::Aggregates,
            "table_functions" | "tables" | "table" | "udtf" => Family::TableFunctions,
            "window_functions" | "windows" | "window" => Family::WindowFunctions,
            "types" | "column_types" | "column-types" | "column" => Family::ColumnTypes,
            "operators" | "operator" | "ops" | "op" => Family::Operators,
            "casts" | "cast_rewrites" | "cast" | "cast-rewrites" => Family::CastRewrites,
            "indexes" | "spatial_indexes" | "index" | "spatial-indexes" | "idx" => {
                Family::SpatialIndexes
            }
            "preprocessors" | "preprocessor_patterns" | "prep" | "preprocessor-patterns" => {
                Family::PreprocessorPatterns
            }
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Loaded catalog shape
// ---------------------------------------------------------------------------

/// One row's identity plus its signature hash, if the source
/// exposes one. `display` is what the text report prints — it's
/// usually the SQL-level name, but for compound-keyed families
/// (operators, casts) it's a formatted composite.
#[derive(Clone, Debug)]
struct Entity {
    pk: String,
    signature_hash: Option<String>,
    display: String,
}

#[derive(Clone, Debug, Default)]
struct FamilyRows {
    /// Sorted by pk for deterministic reports.
    rows: Vec<Entity>,
    /// True when the source shape carries no signature_hash for
    /// this family (TOML input, or a v3 DB for a v4-only family).
    signatures_unavailable: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SourceKind {
    Sqlite,
    Toml,
}

impl SourceKind {
    fn label(self) -> &'static str {
        match self {
            SourceKind::Sqlite => "sqlite",
            SourceKind::Toml => "toml",
        }
    }
}

#[derive(Clone, Debug)]
struct Catalog {
    /// User-facing label — extension name + "@" + version. Used
    /// in the text report header.
    display_label: String,
    extension: String,
    version: String,
    /// SQLite `PRAGMA user_version`, when known. TOML inputs
    /// leave this `None`.
    schema_version: Option<u32>,
    /// Path we loaded from — surfaced in warnings.
    #[allow(dead_code)]
    source_path: PathBuf,
    /// Which loader path produced this catalog. Used to warn on
    /// mixed inputs, where compound PKs (operators, casts) render
    /// with format differences that produce false positives.
    source_kind: SourceKind,
    /// Per-family rows, always populated (empty if the family is
    /// absent from the source).
    families: BTreeMap<Family, FamilyRows>,
}

impl Catalog {
    fn label(&self) -> &str {
        &self.display_label
    }
}

// ---------------------------------------------------------------------------
// Load dispatch
// ---------------------------------------------------------------------------

/// Sniff the input by extension then contents. `.toml` files are
/// parsed as catalog TOML; everything else is opened as SQLite.
/// (Being liberal here matters — CI often names dumps `.db`,
/// `.sqlite`, `.sqlite3`, or with no extension.)
fn load_catalog(path: &Path) -> Result<Catalog> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "toml" {
        load_toml_catalog(path)
    } else {
        load_sqlite_catalog(path)
    }
}

// ---------------------------------------------------------------------------
// SQLite loader
// ---------------------------------------------------------------------------

fn load_sqlite_catalog(path: &Path) -> Result<Catalog> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {}", path.display()))?;

    let user_version: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);

    let (extension, version) = pick_extension_row(&conn)?;

    let mut families: BTreeMap<Family, FamilyRows> = BTreeMap::new();
    for &fam in Family::ALL {
        let rows = load_sqlite_family(&conn, fam, &extension, user_version)
            .with_context(|| format!("loading family {:?}", fam))?;
        families.insert(fam, rows);
    }

    Ok(Catalog {
        display_label: format!("{}@{}", extension, version),
        extension,
        version,
        schema_version: Some(user_version),
        source_path: path.to_path_buf(),
        source_kind: SourceKind::Sqlite,
        families,
    })
}

/// Pick a single `(extension, version)` to head the report. When
/// the DB holds multiple extensions we pick the lexicographically
/// smallest name so the choice is deterministic; the family loads
/// still walk every extension's rows, so no data is dropped.
fn pick_extension_row(conn: &Connection) -> Result<(String, String)> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT name, version FROM extensions ORDER BY name LIMIT 1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .ok();
    row.ok_or_else(|| anyhow!("no rows in `extensions` table — is this a shim-interface DB?"))
}

/// True when `table` has a column named `column`. Used to detect
/// v3-era tables that pre-date B4's `signature_hash`.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    // PRAGMA table_info(?) doesn't bind — build the query as a
    // literal after quoting the table name so a malformed input
    // errors on prepare rather than silently returning `false`.
    let stmt = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
    let mut s = conn.prepare(&stmt)?;
    let mut rows = s.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn load_sqlite_family(
    conn: &Connection,
    fam: Family,
    _extension_hint: &str,
    user_version: u32,
) -> Result<FamilyRows> {
    match fam {
        Family::Scalars => load_simple_named(conn, "scalars", true),
        Family::Aggregates => load_simple_named(conn, "aggregates", true),
        Family::TableFunctions => load_simple_named(conn, "table_functions", true),
        Family::WindowFunctions => load_simple_named(conn, "window_functions", true),
        Family::ColumnTypes => load_column_types(conn, user_version),
        Family::Operators => load_operators(conn, user_version),
        Family::CastRewrites => load_cast_rewrites(conn, user_version),
        Family::SpatialIndexes => load_spatial_indexes(conn, user_version),
        Family::PreprocessorPatterns => load_preprocessor_patterns(conn, user_version),
    }
}

/// Function-shaped tables (scalars/aggregates/table/window):
/// primary key = `(extension, name)`; the diff key is
/// `extension|name` so cross-extension collisions don't merge.
fn load_simple_named(
    conn: &Connection,
    table: &str,
    expect_hash: bool,
) -> Result<FamilyRows> {
    let has_hash = column_exists(conn, table, "signature_hash")?;
    let sql = if has_hash {
        format!("SELECT extension, name, signature_hash FROM {table}")
    } else {
        format!("SELECT extension, name, NULL AS signature_hash FROM {table}")
    };
    let mut s = conn.prepare(&sql)?;
    let iter = s.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut rows: Vec<Entity> = Vec::new();
    let mut any_hash = false;
    for row in iter {
        let (ext, name, hash) = row?;
        any_hash |= hash.is_some();
        rows.push(Entity {
            pk: format!("{ext}|{name}"),
            display: name,
            signature_hash: hash,
        });
    }
    rows.sort_by(|a, b| a.pk.cmp(&b.pk));
    Ok(FamilyRows {
        rows,
        // A v3 DB *should* carry hashes here (B0). If the column
        // exists but every row is null, treat that as "hashes not
        // computed yet" so downstream reports don't lie about
        // having matched by signature.
        signatures_unavailable: !has_hash || (expect_hash && !any_hash),
    })
}

fn load_column_types(conn: &Connection, _user_version: u32) -> Result<FamilyRows> {
    let has_hash = column_exists(conn, "column_types", "signature_hash")?;
    let sql = if has_hash {
        "SELECT extension, type_id, type_name, signature_hash FROM column_types"
    } else {
        "SELECT extension, type_id, type_name, NULL AS signature_hash FROM column_types"
    };
    let mut s = conn.prepare(sql)?;
    let iter = s.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut rows = Vec::new();
    let mut any_hash = false;
    for row in iter {
        let (ext, type_id, type_name, hash) = row?;
        any_hash |= hash.is_some();
        rows.push(Entity {
            pk: format!("{ext}|{type_id}|{type_name}"),
            display: type_name,
            signature_hash: hash,
        });
    }
    rows.sort_by(|a, b| a.pk.cmp(&b.pk));
    Ok(FamilyRows {
        rows,
        signatures_unavailable: !has_hash || !any_hash,
    })
}

fn load_operators(conn: &Connection, _user_version: u32) -> Result<FamilyRows> {
    let has_hash = column_exists(conn, "operators", "signature_hash")?;
    let sql = if has_hash {
        "SELECT extension, symbol, lhs_type_id, rhs_type_id, function_name, signature_hash \
         FROM operators"
    } else {
        "SELECT extension, symbol, lhs_type_id, rhs_type_id, function_name, NULL AS signature_hash \
         FROM operators"
    };
    let mut s = conn.prepare(sql)?;
    let iter = s.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut rows = Vec::new();
    let mut any_hash = false;
    for row in iter {
        let (ext, symbol, lhs, rhs, fn_name, hash) = row?;
        any_hash |= hash.is_some();
        let lhs_s = lhs
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".to_string());
        let rhs_s = rhs
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".to_string());
        rows.push(Entity {
            pk: format!("{ext}|{symbol}|{lhs_s}|{rhs_s}"),
            display: format!("{symbol} ({lhs_s},{rhs_s}) -> {fn_name}"),
            signature_hash: hash,
        });
    }
    rows.sort_by(|a, b| a.pk.cmp(&b.pk));
    Ok(FamilyRows {
        rows,
        signatures_unavailable: !has_hash || !any_hash,
    })
}

fn load_cast_rewrites(conn: &Connection, _user_version: u32) -> Result<FamilyRows> {
    let has_hash = column_exists(conn, "cast_rewrites", "signature_hash")?;
    let sql = if has_hash {
        "SELECT extension, target_type, source_kind, source_fn_hint, source_type_id, \
                function_name, signature_hash \
         FROM cast_rewrites"
    } else {
        "SELECT extension, target_type, source_kind, source_fn_hint, source_type_id, \
                function_name, NULL AS signature_hash \
         FROM cast_rewrites"
    };
    let mut s = conn.prepare(sql)?;
    let iter = s.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })?;
    let mut rows = Vec::new();
    let mut any_hash = false;
    for row in iter {
        let (ext, tgt, kind, hint, stid, fn_name, hash) = row?;
        any_hash |= hash.is_some();
        rows.push(Entity {
            pk: format!("{ext}|{tgt}|{kind}|{hint}|{stid}"),
            display: format!("{tgt} <- {kind}/{hint}#{stid} via {fn_name}"),
            signature_hash: hash,
        });
    }
    rows.sort_by(|a, b| a.pk.cmp(&b.pk));
    Ok(FamilyRows {
        rows,
        signatures_unavailable: !has_hash || !any_hash,
    })
}

fn load_spatial_indexes(conn: &Connection, _user_version: u32) -> Result<FamilyRows> {
    let has_hash = column_exists(conn, "spatial_indexes", "signature_hash")?;
    let sql = if has_hash {
        "SELECT extension, name, type_id, signature_hash FROM spatial_indexes"
    } else {
        "SELECT extension, name, type_id, NULL AS signature_hash FROM spatial_indexes"
    };
    let mut s = conn.prepare(sql)?;
    let iter = s.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut rows = Vec::new();
    let mut any_hash = false;
    for row in iter {
        let (ext, name, type_id, hash) = row?;
        any_hash |= hash.is_some();
        rows.push(Entity {
            pk: format!("{ext}|{name}|{type_id}"),
            display: name,
            signature_hash: hash,
        });
    }
    rows.sort_by(|a, b| a.pk.cmp(&b.pk));
    Ok(FamilyRows {
        rows,
        signatures_unavailable: !has_hash || !any_hash,
    })
}

fn load_preprocessor_patterns(conn: &Connection, _user_version: u32) -> Result<FamilyRows> {
    let has_hash = column_exists(conn, "preprocessor_patterns", "signature_hash")?;
    let sql = if has_hash {
        "SELECT extension, op_token, function_name, signature_hash FROM preprocessor_patterns"
    } else {
        "SELECT extension, op_token, function_name, NULL AS signature_hash \
         FROM preprocessor_patterns"
    };
    let mut s = conn.prepare(sql)?;
    let iter = s.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut rows = Vec::new();
    let mut any_hash = false;
    for row in iter {
        let (ext, op_token, fn_name, hash) = row?;
        any_hash |= hash.is_some();
        rows.push(Entity {
            pk: format!("{ext}|{op_token}"),
            display: format!("{op_token} -> {fn_name}"),
            signature_hash: hash,
        });
    }
    rows.sort_by(|a, b| a.pk.cmp(&b.pk));
    Ok(FamilyRows {
        rows,
        signatures_unavailable: !has_hash || !any_hash,
    })
}

// ---------------------------------------------------------------------------
// TOML loader
// ---------------------------------------------------------------------------

/// Wire shape of the emitted `<extension>-catalog.toml`. We only
/// pull out the fields diff cares about — the emitter's richer
/// metadata (leaves, umbrellas) is folded into the flat entity
/// list here.
#[derive(Debug, Deserialize)]
struct CatalogToml {
    #[serde(default)]
    meta: TomlMeta,
    #[serde(default)]
    #[serde(rename = "types")]
    types: Vec<TomlType>,
    #[serde(default)]
    leaves: Vec<TomlLeaf>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlMeta {
    #[serde(default)]
    extension: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TomlType {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    kind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlLeaf {
    #[serde(default)]
    scalars: Vec<String>,
    #[serde(default)]
    aggregates: Vec<String>,
    #[serde(default)]
    table_functions: Vec<String>,
    #[serde(default)]
    window_functions: Vec<String>,
    #[serde(default)]
    operators: Vec<String>,
    #[serde(default)]
    casts: Vec<String>,
    #[serde(default)]
    preprocessor_patterns: Vec<String>,
    #[serde(default)]
    spatial_indexes: Vec<String>,
}

fn load_toml_catalog(path: &Path) -> Result<Catalog> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let doc: CatalogToml = toml::from_str(&text)
        .with_context(|| format!("parsing TOML {}", path.display()))?;

    let extension = doc
        .meta
        .extension
        .clone()
        .unwrap_or_else(|| "<unknown-extension>".to_string());
    let version = doc
        .meta
        .version
        .clone()
        .unwrap_or_else(|| "<unknown-version>".to_string());

    // Union all leaves' name lists into per-family sets. The
    // emitter guarantees deterministic ordering already; we
    // re-sort defensively so the diff key sort is total.
    let mut per_family: BTreeMap<Family, BTreeSet<String>> = BTreeMap::new();
    for leaf in &doc.leaves {
        for name in &leaf.scalars {
            per_family.entry(Family::Scalars).or_default().insert(name.clone());
        }
        for name in &leaf.aggregates {
            per_family.entry(Family::Aggregates).or_default().insert(name.clone());
        }
        for name in &leaf.table_functions {
            per_family.entry(Family::TableFunctions).or_default().insert(name.clone());
        }
        for name in &leaf.window_functions {
            per_family.entry(Family::WindowFunctions).or_default().insert(name.clone());
        }
        for name in &leaf.operators {
            per_family.entry(Family::Operators).or_default().insert(name.clone());
        }
        for name in &leaf.casts {
            per_family.entry(Family::CastRewrites).or_default().insert(name.clone());
        }
        for name in &leaf.preprocessor_patterns {
            per_family
                .entry(Family::PreprocessorPatterns)
                .or_default()
                .insert(name.clone());
        }
        for name in &leaf.spatial_indexes {
            per_family
                .entry(Family::SpatialIndexes)
                .or_default()
                .insert(name.clone());
        }
    }
    // types come from a top-level [[types]] array.
    for t in &doc.types {
        per_family
            .entry(Family::ColumnTypes)
            .or_default()
            .insert(t.name.clone());
    }

    let mut families: BTreeMap<Family, FamilyRows> = BTreeMap::new();
    for &fam in Family::ALL {
        let names = per_family.remove(&fam).unwrap_or_default();
        let rows: Vec<Entity> = names
            .into_iter()
            .map(|n| Entity {
                pk: format!("{extension}|{n}"),
                display: n,
                signature_hash: None,
            })
            .collect();
        families.insert(
            fam,
            FamilyRows {
                rows,
                // TOML never carries per-row signatures.
                signatures_unavailable: true,
            },
        );
    }

    Ok(Catalog {
        display_label: format!("{}@{}", extension, version),
        extension,
        version,
        schema_version: None,
        source_path: path.to_path_buf(),
        source_kind: SourceKind::Toml,
        families,
    })
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
struct EntityRef {
    pk: String,
    display: String,
}

impl EntityRef {
    fn from(e: &Entity) -> Self {
        EntityRef {
            pk: e.pk.clone(),
            display: e.display.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct SignatureChange {
    pk: String,
    display: String,
    before_hash: Option<String>,
    after_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct FamilyDelta {
    family: &'static str,
    added: Vec<EntityRef>,
    removed: Vec<EntityRef>,
    signature_changed: Vec<SignatureChange>,
    unchanged: usize,
    /// True when we could not compare by signature hash (either
    /// side was missing hashes). Reported to the human so they
    /// know "unchanged" is name-only, not hash-checked.
    hash_compare_skipped: bool,
    /// Only true when both sides participated in the compare —
    /// used by the classifier to decide whether "no change" is
    /// authoritative for this family.
    compared: bool,
    /// Reason for skipping (surfaced in text output).
    skip_reason: Option<String>,
}

fn diff_family(fam: Family, before: &FamilyRows, after: &FamilyRows) -> FamilyDelta {
    let before_by_pk: BTreeMap<&str, &Entity> = before
        .rows
        .iter()
        .map(|e| (e.pk.as_str(), e))
        .collect();
    let after_by_pk: BTreeMap<&str, &Entity> = after
        .rows
        .iter()
        .map(|e| (e.pk.as_str(), e))
        .collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut sig_changed = Vec::new();
    let mut unchanged: usize = 0;

    let hash_compare_skipped = before.signatures_unavailable || after.signatures_unavailable;

    for (pk, a) in &after_by_pk {
        match before_by_pk.get(pk) {
            None => added.push(EntityRef::from(a)),
            Some(b) => {
                if hash_compare_skipped {
                    unchanged += 1;
                } else {
                    match (&b.signature_hash, &a.signature_hash) {
                        (Some(bh), Some(ah)) if bh == ah => unchanged += 1,
                        (Some(_), Some(_)) => sig_changed.push(SignatureChange {
                            pk: (*pk).to_string(),
                            display: a.display.clone(),
                            before_hash: b.signature_hash.clone(),
                            after_hash: a.signature_hash.clone(),
                        }),
                        // One side null, other not, or both null:
                        // we still say "unchanged" but note the
                        // partial compare via skip_reason. This is
                        // narrower than the loader-level guard.
                        _ => unchanged += 1,
                    }
                }
            }
        }
    }
    for (pk, b) in &before_by_pk {
        if !after_by_pk.contains_key(pk) {
            removed.push(EntityRef::from(b));
        }
    }

    // Deterministic ordering (loaders already sorted, but the
    // merge loop above is a BTreeMap walk on pk — the produced
    // vecs are pk-sorted, which mirrors the display sort for
    // human-friendly output).
    added.sort_by(|a, b| a.pk.cmp(&b.pk));
    removed.sort_by(|a, b| a.pk.cmp(&b.pk));
    sig_changed.sort_by(|a, b| a.pk.cmp(&b.pk));

    let skip_reason = if hash_compare_skipped {
        Some(match (
            before.signatures_unavailable,
            after.signatures_unavailable,
        ) {
            (true, true) => "signature_hash unavailable on both sides".to_string(),
            (true, false) => "signature_hash unavailable on before".to_string(),
            (false, true) => "signature_hash unavailable on after".to_string(),
            (false, false) => unreachable!(),
        })
    } else {
        None
    };

    FamilyDelta {
        family: fam.slug(),
        added,
        removed,
        signature_changed: sig_changed,
        unchanged,
        hash_compare_skipped,
        compared: true,
        skip_reason,
    }
}

// ---------------------------------------------------------------------------
// SEMVER classifier
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Semver {
    Major,
    Minor,
    MinorWithChanges,
    Patch,
}

impl Semver {
    fn label(self) -> &'static str {
        match self {
            Semver::Major => "MAJOR",
            Semver::Minor => "MINOR",
            Semver::MinorWithChanges => "MINOR-WITH-CHANGES",
            Semver::Patch => "PATCH",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Semver::Major => "removals present — breaking",
            Semver::Minor => "additive only, no breaking removals",
            Semver::MinorWithChanges => "signature changes flagged for re-verification",
            Semver::Patch => "no observable delta",
        }
    }
}

fn classify(deltas: &[FamilyDelta]) -> Semver {
    let any_removed = deltas.iter().any(|d| !d.removed.is_empty());
    let any_added = deltas.iter().any(|d| !d.added.is_empty());
    let any_sig = deltas.iter().any(|d| !d.signature_changed.is_empty());
    if any_removed {
        Semver::Major
    } else if any_sig {
        Semver::MinorWithChanges
    } else if any_added {
        Semver::Minor
    } else {
        Semver::Patch
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
struct Report {
    before: CatalogSummary,
    after: CatalogSummary,
    families: Vec<FamilyDelta>,
    semver: SemverBlock,
    warnings: Vec<String>,
    human_gate: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct CatalogSummary {
    label: String,
    extension: String,
    version: String,
    schema_version: Option<u32>,
    source_path: String,
}

#[derive(Clone, Debug, Serialize)]
struct SemverBlock {
    class: Semver,
    reason: String,
}

fn build_report(
    before: &Catalog,
    after: &Catalog,
    filter: &BTreeSet<Family>,
) -> Report {
    let mut families = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut human_gate: Vec<String> = Vec::new();

    // v3-only warning: user has a DB predating B4.
    for (side, cat) in [("before", before), ("after", after)] {
        if let Some(sv) = cat.schema_version {
            if sv < 4 {
                warnings.push(format!(
                    "{side} DB is at schema v{sv} (< v4); five of the nine \
                     families won't carry signature_hash — compare falls back \
                     to name-only for those. Apply B4 to enable hash-level diff.",
                ));
            }
        }
    }
    if before.extension != after.extension {
        warnings.push(format!(
            "extension mismatch: before='{}' vs after='{}' — diffing anyway, \
             but PK collisions between different extensions may look like \
             additions/removals rather than the extension swap they are.",
            before.extension, after.extension
        ));
    }
    if before.source_kind != after.source_kind {
        warnings.push(format!(
            "mixed input formats (before={}, after={}) — compound-keyed \
             families (operators, casts, types) use different PK renderings \
             per format, so the diff will show them as removed-and-re-added. \
             Compare like formats to avoid the noise.",
            before.source_kind.label(),
            after.source_kind.label(),
        ));
    }

    for &fam in Family::ALL {
        if !filter.is_empty() && !filter.contains(&fam) {
            continue;
        }
        let empty = FamilyRows::default();
        let b_rows = before.families.get(&fam).unwrap_or(&empty);
        let a_rows = after.families.get(&fam).unwrap_or(&empty);
        let delta = diff_family(fam, b_rows, a_rows);
        // Human-gate hints.
        if !delta.added.is_empty() {
            human_gate.push(format!(
                "{count} new {fam} need test cases",
                count = delta.added.len(),
                fam = fam.label().to_lowercase()
            ));
        }
        if !delta.signature_changed.is_empty() {
            human_gate.push(format!(
                "{count} {fam} with changed signatures need re-verification",
                count = delta.signature_changed.len(),
                fam = fam.label().to_lowercase()
            ));
        }
        // v3-era note surfaced per-family too.
        if delta.hash_compare_skipped
            && (before.schema_version.unwrap_or(u32::MAX) < 4
                || after.schema_version.unwrap_or(u32::MAX) < 4)
            && !fam.has_hash_at_v3()
        {
            // Already covered by the top-level warning — no
            // per-family duplication needed.
        }
        families.push(delta);
    }

    let sv = classify(&families);
    Report {
        before: CatalogSummary {
            label: before.label().to_string(),
            extension: before.extension.clone(),
            version: before.version.clone(),
            schema_version: before.schema_version,
            source_path: before.source_path.display().to_string(),
        },
        after: CatalogSummary {
            label: after.label().to_string(),
            extension: after.extension.clone(),
            version: after.version.clone(),
            schema_version: after.schema_version,
            source_path: after.source_path.display().to_string(),
        },
        families,
        semver: SemverBlock {
            class: sv,
            reason: sv.reason().to_string(),
        },
        warnings,
        human_gate,
    }
}

// ---------------------------------------------------------------------------
// Text output
// ---------------------------------------------------------------------------

/// Number of sample entities to enumerate under `added` /
/// `removed` / `signature_changed` in the text report. Matches
/// the task spec of "first 20".
const SAMPLE_LIMIT: usize = 20;

/// Render the human-facing text report into a `String`. The main
/// loop routes the result to stdout or to `--text-out` — keeping
/// the formatter side-effect-free lets `--output-format=both`
/// atomically write both artifacts (or fail without a partial
/// stdout dump).
fn format_text(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Comparing {} (before) vs {} (after):",
        report.before.label, report.after.label,
    );
    if let (Some(bv), Some(av)) = (report.before.schema_version, report.after.schema_version) {
        let _ = writeln!(out, "  schema: v{} (before) vs v{} (after)", bv, av);
    }
    let _ = writeln!(out);

    for delta in &report.families {
        format_family_text(&mut out, delta);
    }

    if !report.warnings.is_empty() {
        let _ = writeln!(out);
        for w in &report.warnings {
            let _ = writeln!(out, "WARNING: {w}");
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "SEMVER classification: {} ({})",
        report.semver.class.label(),
        report.semver.reason
    );
    if !report.human_gate.is_empty() {
        let _ = writeln!(out, "Human gate:");
        for h in &report.human_gate {
            let _ = writeln!(out, "  - {h}");
        }
    }
    out
}

fn format_family_text(out: &mut String, d: &FamilyDelta) {
    let label = Family::parse_slug(d.family)
        .map(|f| f.label())
        .unwrap_or(d.family);
    let n_add = d.added.len();
    let n_rem = d.removed.len();
    let n_sig = d.signature_changed.len();
    let n_unc = d.unchanged;

    let _ = writeln!(
        out,
        "{}: +{} -{} ~{} = {} unchanged",
        label, n_add, n_rem, n_sig, n_unc
    );

    // Detail lines only when there's something to show.
    if n_add > 0 {
        let sample: Vec<String> = d
            .added
            .iter()
            .take(SAMPLE_LIMIT)
            .map(|e| e.display.clone())
            .collect();
        let _ = writeln!(
            out,
            "  +{n_add} added:  {}{}",
            sample.join(", "),
            if n_add > SAMPLE_LIMIT {
                format!(", ... (+{})", n_add - SAMPLE_LIMIT)
            } else {
                String::new()
            }
        );
    }
    if n_rem > 0 {
        let sample: Vec<String> = d
            .removed
            .iter()
            .take(SAMPLE_LIMIT)
            .map(|e| e.display.clone())
            .collect();
        let _ = writeln!(
            out,
            "  -{n_rem} removed:  {}{}",
            sample.join(", "),
            if n_rem > SAMPLE_LIMIT {
                format!(", ... (-{})", n_rem - SAMPLE_LIMIT)
            } else {
                String::new()
            }
        );
    }
    if n_sig > 0 {
        let sample: Vec<String> = d
            .signature_changed
            .iter()
            .take(SAMPLE_LIMIT)
            .map(|s| s.display.clone())
            .collect();
        let _ = writeln!(
            out,
            "  ~{n_sig} signature-changed:  {}{}",
            sample.join(", "),
            if n_sig > SAMPLE_LIMIT {
                format!(", ... (~{})", n_sig - SAMPLE_LIMIT)
            } else {
                String::new()
            }
        );
    }
    if d.hash_compare_skipped {
        if let Some(reason) = &d.skip_reason {
            let _ = writeln!(out, "  ! hash-level compare skipped: {reason}");
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter: BTreeSet<Family> = if cli.entity_filter.is_empty() {
        BTreeSet::new() // treated as "no filter" by build_report
    } else {
        let mut set = BTreeSet::new();
        for raw in &cli.entity_filter {
            let fam = Family::parse_slug(raw)
                .ok_or_else(|| anyhow!("unknown --entity-filter value: {raw}"))?;
            set.insert(fam);
        }
        set
    };

    let before = load_catalog(&cli.before)
        .with_context(|| format!("loading --before {}", cli.before.display()))?;
    let after = load_catalog(&cli.after)
        .with_context(|| format!("loading --after {}", cli.after.display()))?;

    let report = build_report(&before, &after, &filter);

    // Resolve the effective output format from `--output-format`
    // plus the `--text-out` / `--json-out` implication rules.
    // When the user doesn't pass `--output-format`, having both
    // output paths set means "both formats" (single-shot dual
    // emission, exactly what bump-upstream's `step_diff` wants).
    let effective = match cli.output_format {
        Some(fmt) => fmt,
        None => match (cli.text_out.is_some(), cli.json_out.is_some()) {
            (true, true) => OutputFormat::Both,
            (true, false) => OutputFormat::Text,
            (false, true) => OutputFormat::Json,
            (false, false) => OutputFormat::Text,
        },
    };

    // `both` needs somewhere to put each stream — otherwise text
    // and JSON would both fight for stdout. Fail early rather
    // than emit a garbled mix.
    if effective == OutputFormat::Both
        && (cli.text_out.is_none() || cli.json_out.is_none())
    {
        bail!(
            "--output-format=both requires both --text-out and --json-out \
             (stdout cannot carry two formats simultaneously)"
        );
    }

    let want_text = matches!(effective, OutputFormat::Text | OutputFormat::Both);
    let want_json = matches!(effective, OutputFormat::Json | OutputFormat::Both);

    // Render both artifacts first, then commit them — an error in
    // either serialisation aborts before anything reaches disk or
    // stdout, matching the atomic contract cascade steps expect.
    let text_body = if want_text { Some(format_text(&report)) } else { None };
    let json_body = if want_json {
        Some(
            serde_json::to_string_pretty(&report)
                .context("serialising JSON report")?,
        )
    } else {
        None
    };

    if let Some(body) = text_body {
        match &cli.text_out {
            Some(path) => fs::write(path, &body)
                .with_context(|| format!("writing text report to {}", path.display()))?,
            None => print!("{}", body),
        }
    }
    if let Some(body) = json_body {
        match &cli.json_out {
            Some(path) => fs::write(path, &body)
                .with_context(|| format!("writing JSON delta to {}", path.display()))?,
            None => println!("{}", body),
        }
    }

    // Exit code non-zero when the diff is MAJOR so CI pipelines
    // can gate on removals without parsing the report. Keep 0
    // for MINOR/PATCH so incremental releases stay green.
    if matches!(report.semver.class, Semver::Major) {
        bail!("MAJOR — removals detected (see report)");
    }
    Ok(())
}
