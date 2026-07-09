//! blake3 hashing for function signatures and implementations.
//!
//! Two hashes per function row:
//!   - `signature_hash` -- SQL-visible surface only (name +
//!     canonicalised `param_types_json` + return type + trait
//!     flags). Two shims producing the same signature hash are
//!     interchangeable from a caller's point of view.
//!   - `implementation_hash` -- bytes of the owner Rust file
//!     plus a folded blake3 of every helper under
//!     `src/helpers/**/*.rs`. Reads intentionally NOT semantic:
//!     comment-only churn re-hashes; that's what backfill's
//!     `--recompute-hashes` flag is for.
//!
//! Serialisation is canonical JSON (BTreeMap-ordered keys, nested
//! JSON strings re-canonicalised) so `blake3sum <<< '<json>'`
//! reproduces the hash from the command line.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde_json::Value;

/// Serialise a slice of JSON values into canonical bytes:
///  - top-level array preserving the caller's ordering (position
///    is meaningful; that's the signature schema).
///  - every nested object recursively re-keyed with BTreeMap
///    ordering.
///  - nested JSON strings (`param_types_json`,
///    `config_arg_indices_json`) are parsed and re-canonicalised
///    so whitespace / key-order drift doesn't leak into the hash.
pub fn canonical_json_bytes(values: &[Value]) -> Result<Vec<u8>> {
    let mut arr: Vec<Value> = Vec::with_capacity(values.len());
    for v in values {
        arr.push(canonicalize(v.clone())?);
    }
    Ok(serde_json::to_vec(&Value::Array(arr))?)
}

fn canonicalize(v: Value) -> Result<Value> {
    Ok(match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            for k in keys {
                out.insert(k.clone(), canonicalize(map[&k].clone())?);
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(canonicalize(it)?);
            }
            Value::Array(out)
        }
        other => other,
    })
}

/// Re-canonicalise a JSON-shaped string. Falls back to wrapping
/// the raw string as a JSON string value if the payload doesn't
/// parse (defensive -- lets the hash still be computed against
/// malformed catalog rows, at the cost of the hash being
/// whitespace-sensitive for that one row).
pub fn recanonicalize_json_str(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => canonicalize(v).unwrap_or(Value::String(raw.to_string())),
        Err(_) => Value::String(raw.to_string()),
    }
}

/// A scalar's SQL surface, minus the extension namespace (rename
/// the shim = don't churn every hash).
pub struct ScalarSig<'a> {
    pub name: &'a str,
    pub param_types_json: &'a str,
    pub return_type: &'a str,
    pub is_deterministic: bool,
    pub propagates_null: bool,
}
pub fn scalar_signature_hash(s: &ScalarSig<'_>) -> String {
    let bytes = canonical_json_bytes(&[
        Value::String(s.name.to_string()),
        recanonicalize_json_str(s.param_types_json),
        Value::String(s.return_type.to_string()),
        Value::Bool(s.is_deterministic),
        Value::Bool(s.propagates_null),
    ])
    .expect("signature json is finite");
    blake3::hash(&bytes).to_hex().to_string()
}

pub struct AggregateSig<'a> {
    pub name: &'a str,
    pub param_types_json: &'a str,
    pub supports_grouped: bool,
    pub supports_partial: bool,
    pub is_order_sensitive: bool,
    pub accepts_config: bool,
    pub config_arg_indices_json: &'a str,
}
pub fn aggregate_signature_hash(a: &AggregateSig<'_>) -> String {
    let bytes = canonical_json_bytes(&[
        Value::String(a.name.to_string()),
        recanonicalize_json_str(a.param_types_json),
        Value::Bool(a.supports_grouped),
        Value::Bool(a.supports_partial),
        Value::Bool(a.is_order_sensitive),
        Value::Bool(a.accepts_config),
        recanonicalize_json_str(a.config_arg_indices_json),
    ])
    .expect("signature json is finite");
    blake3::hash(&bytes).to_hex().to_string()
}

pub struct SimpleFnSig<'a> {
    pub name: &'a str,
    pub param_types_json: &'a str,
}
pub fn table_function_signature_hash(t: &SimpleFnSig<'_>) -> String {
    simple_fn_signature_hash("table_function", t)
}
pub fn window_function_signature_hash(t: &SimpleFnSig<'_>) -> String {
    simple_fn_signature_hash("window_function", t)
}

fn simple_fn_signature_hash(kind: &str, t: &SimpleFnSig<'_>) -> String {
    let bytes = canonical_json_bytes(&[
        Value::String(kind.to_string()),
        Value::String(t.name.to_string()),
        recanonicalize_json_str(t.param_types_json),
    ])
    .expect("signature json is finite");
    blake3::hash(&bytes).to_hex().to_string()
}

// ---------------------------------------------------------------------------
// B4 (2026-07-09): signature hashes for the five non-function
// catalog tables. No `implementation_hash` counterpart — types /
// operators / casts / spatial indexes / preprocessor patterns
// don't map to a source module; the signature IS the identity.
//
// Each hash is prefixed with the kind tag so a
// `blake3sum <<< '<json>'` reproduction from the CLI still
// distinguishes rows of different kinds that happen to share
// their identifying columns.
// ---------------------------------------------------------------------------

/// A column-type row's SQL-visible identity: type name, storage
/// size, and cast_from / cast_to JSON payloads.
pub struct ColumnTypeSig<'a> {
    pub type_name: &'a str,
    pub storage_size: i64,
    pub cast_from_json: &'a str,
    pub cast_to_json: &'a str,
}
pub fn column_type_signature_hash(t: &ColumnTypeSig<'_>) -> String {
    let bytes = canonical_json_bytes(&[
        Value::String("column_type".to_string()),
        Value::String(t.type_name.to_string()),
        Value::Number(t.storage_size.into()),
        recanonicalize_json_str(t.cast_from_json),
        recanonicalize_json_str(t.cast_to_json),
    ])
    .expect("signature json is finite");
    blake3::hash(&bytes).to_hex().to_string()
}

/// An operator's SQL-visible identity: infix symbol, both side
/// type ids (nullable) and the backing SQL function name. There
/// is no separate `result_type` column in the shim-interface
/// catalog yet; when one lands, extend the tuple here rather
/// than folding it into `backing_function`.
pub struct OperatorSig<'a> {
    pub symbol: &'a str,
    pub lhs_type_id: Option<i64>,
    pub rhs_type_id: Option<i64>,
    pub backing_function: &'a str,
}
pub fn operator_signature_hash(o: &OperatorSig<'_>) -> String {
    let lhs = match o.lhs_type_id {
        Some(n) => Value::Number(n.into()),
        None => Value::Null,
    };
    let rhs = match o.rhs_type_id {
        Some(n) => Value::Number(n.into()),
        None => Value::Null,
    };
    let bytes = canonical_json_bytes(&[
        Value::String("operator".to_string()),
        Value::String(o.symbol.to_string()),
        lhs,
        rhs,
        Value::String(o.backing_function.to_string()),
    ])
    .expect("signature json is finite");
    blake3::hash(&bytes).to_hex().to_string()
}

/// A cast-rewrite row's SQL-visible identity: `source_type_id`,
/// target type name, source-kind discriminant, and the rewrite
/// target (backing SQL function). `source_fn_hint` is folded in
/// as an extension of the source-side discriminant so casts that
/// share a `(target, source_kind, source_type_id)` but differ in
/// their source-side function still get distinct hashes.
pub struct CastRewriteSig<'a> {
    pub source_type_id: i64,
    pub target_type: &'a str,
    pub source_kind: &'a str,
    pub source_fn_hint: &'a str,
    pub rewrite_target: &'a str,
}
pub fn cast_rewrite_signature_hash(c: &CastRewriteSig<'_>) -> String {
    let bytes = canonical_json_bytes(&[
        Value::String("cast_rewrite".to_string()),
        Value::Number(c.source_type_id.into()),
        Value::String(c.target_type.to_string()),
        Value::String(c.source_kind.to_string()),
        Value::String(c.source_fn_hint.to_string()),
        Value::String(c.rewrite_target.to_string()),
    ])
    .expect("signature json is finite");
    blake3::hash(&bytes).to_hex().to_string()
}

/// A spatial-index row's SQL-visible identity: the shim-side
/// method name (used as the `name` PK column) plus the
/// capabilities JSON blob (which surfaces the supported
/// operators / KNN / within-distance flags). The task-side
/// name for the JSON is `supported_types_json`; the catalog
/// column is `capabilities_json` — they're the same payload.
pub struct SpatialIndexSig<'a> {
    pub method: &'a str,
    pub capabilities_json: &'a str,
}
pub fn spatial_index_signature_hash(s: &SpatialIndexSig<'_>) -> String {
    // `capabilities_json` is nullable in the catalog (path-#1
    // `index-plugin` registrations don't carry it); treat NULL
    // as the empty JSON object so the hash stays stable.
    let caps = if s.capabilities_json.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        recanonicalize_json_str(s.capabilities_json)
    };
    let bytes = canonical_json_bytes(&[
        Value::String("spatial_index".to_string()),
        Value::String(s.method.to_string()),
        caps,
    ])
    .expect("signature json is finite");
    blake3::hash(&bytes).to_hex().to_string()
}

/// A preprocessor-pattern row's SQL-visible identity: the
/// operator token that triggers the rewrite plus the SQL
/// function it rewrites to.
pub struct PreprocessorPatternSig<'a> {
    pub op_token: &'a str,
    pub function_name: &'a str,
}
pub fn preprocessor_pattern_signature_hash(p: &PreprocessorPatternSig<'_>) -> String {
    let bytes = canonical_json_bytes(&[
        Value::String("preprocessor_pattern".to_string()),
        Value::String(p.op_token.to_string()),
        Value::String(p.function_name.to_string()),
    ])
    .expect("signature json is finite");
    blake3::hash(&bytes).to_hex().to_string()
}

/// Deterministic blake3 over every `.rs` file under
/// `helpers_root`. Alphabetical file traversal + relative-path
/// bytes + `\0` boundary + file bytes. Cached per-process via a
/// [`OnceLock`] keyed by absolute path.
pub fn helpers_hash(helpers_root: &Path) -> Result<[u8; 32]> {
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, [u8; 32]>>> =
        OnceLock::new();
    let map = CACHE.get_or_init(Default::default);
    let key = helpers_root.canonicalize().unwrap_or_else(|_| helpers_root.to_path_buf());
    {
        let g = map.lock().unwrap();
        if let Some(h) = g.get(&key) {
            return Ok(*h);
        }
    }
    let mut hasher = blake3::Hasher::new();
    if helpers_root.exists() {
        let mut entries: Vec<_> = walkdir::WalkDir::new(helpers_root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_type().is_file() && e.path().extension() == Some(OsStr::new("rs"))
            })
            .collect();
        entries.sort_by(|a, b| a.path().cmp(b.path()));
        for entry in entries {
            let rel = entry
                .path()
                .strip_prefix(helpers_root)
                .unwrap_or_else(|_| entry.path());
            hasher.update(rel.to_string_lossy().as_bytes());
            hasher.update(&[0u8]);
            let bytes = fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            hasher.update(&bytes);
        }
    }
    let hash = *hasher.finalize().as_bytes();
    let mut g = map.lock().unwrap();
    g.insert(key, hash);
    Ok(hash)
}

/// blake3 of the owner Rust file, folded with the helpers hash.
/// Owner-file resolution is a per-shim concern; callers pass the
/// resolved path in. `owner_file` may be `None` when the owner
/// map returns no mapping for the row's `interface` -- in that
/// case only `helpers_hash` folds into the result.
pub fn implementation_hash(
    owner_file: Option<&Path>,
    helpers_hash: &[u8; 32],
) -> Result<String> {
    let mut h = blake3::Hasher::new();
    if let Some(path) = owner_file {
        let owner_bytes = fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        h.update(&owner_bytes);
    }
    h.update(helpers_hash);
    Ok(h.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_signature_hash_is_stable() {
        let a = ScalarSig {
            name: "st_area",
            param_types_json: "[[\"geometry\"]]",
            return_type: "double",
            is_deterministic: true,
            propagates_null: true,
        };
        let b = ScalarSig {
            name: "st_area",
            // whitespace + key-order drift should be neutralised
            // (json is a JSON array here so ordering IS meaningful
            // for its elements; whitespace inside is not).
            param_types_json: "[[ \"geometry\" ]]",
            return_type: "double",
            is_deterministic: true,
            propagates_null: true,
        };
        assert_eq!(scalar_signature_hash(&a), scalar_signature_hash(&b));
    }

    #[test]
    fn scalar_signature_hash_reflects_determinism() {
        let a = ScalarSig {
            name: "st_area",
            param_types_json: "[[\"geometry\"]]",
            return_type: "double",
            is_deterministic: true,
            propagates_null: true,
        };
        let b = ScalarSig { is_deterministic: false, ..a };
        assert_ne!(scalar_signature_hash(&a), scalar_signature_hash(&b));
    }

    #[test]
    fn aggregate_signature_hash_reflects_config() {
        let a = AggregateSig {
            name: "st_extent",
            param_types_json: "[[\"geometry\"]]",
            supports_grouped: true,
            supports_partial: true,
            is_order_sensitive: false,
            accepts_config: false,
            config_arg_indices_json: "[]",
        };
        let b = AggregateSig { accepts_config: true, ..a };
        assert_ne!(aggregate_signature_hash(&a), aggregate_signature_hash(&b));
    }

    #[test]
    fn helpers_hash_missing_dir_is_empty_blake3() {
        let tmp = std::env::temp_dir().join("shim_iface_helpers_missing_xyz");
        let h = helpers_hash(&tmp).unwrap();
        assert_eq!(h, *blake3::Hasher::new().finalize().as_bytes());
    }

    #[test]
    fn column_type_hash_reflects_storage_size() {
        let a = ColumnTypeSig {
            type_name: "geometry",
            storage_size: -1,
            cast_from_json: "[]",
            cast_to_json: "[]",
        };
        let b = ColumnTypeSig { storage_size: 128, ..a };
        assert_ne!(column_type_signature_hash(&a), column_type_signature_hash(&b));
    }

    #[test]
    fn column_type_hash_is_stable_across_whitespace() {
        let a = ColumnTypeSig {
            type_name: "geometry",
            storage_size: -1,
            cast_from_json: "[\"text\"]",
            cast_to_json: "[]",
        };
        let b = ColumnTypeSig {
            cast_from_json: "[ \"text\" ]",
            ..a
        };
        assert_eq!(column_type_signature_hash(&a), column_type_signature_hash(&b));
    }

    #[test]
    fn operator_hash_reflects_backing_function() {
        let a = OperatorSig {
            symbol: "&&",
            lhs_type_id: Some(1),
            rhs_type_id: Some(1),
            backing_function: "geometry_overlaps",
        };
        let b = OperatorSig { backing_function: "st_intersects", ..a };
        assert_ne!(operator_signature_hash(&a), operator_signature_hash(&b));
    }

    #[test]
    fn operator_hash_handles_null_sides() {
        let both_null = OperatorSig {
            symbol: "!",
            lhs_type_id: None,
            rhs_type_id: None,
            backing_function: "st_not",
        };
        let lhs_typed = OperatorSig { lhs_type_id: Some(0), ..both_null };
        assert_ne!(
            operator_signature_hash(&both_null),
            operator_signature_hash(&lhs_typed)
        );
    }

    #[test]
    fn cast_rewrite_hash_reflects_rewrite_target() {
        let a = CastRewriteSig {
            source_type_id: 0,
            target_type: "geometry",
            source_kind: "any",
            source_fn_hint: "",
            rewrite_target: "st_geomfromwkb",
        };
        let b = CastRewriteSig { rewrite_target: "st_geomfromtext", ..a };
        assert_ne!(cast_rewrite_signature_hash(&a), cast_rewrite_signature_hash(&b));
    }

    #[test]
    fn spatial_index_hash_treats_empty_caps_as_empty_object() {
        let a = SpatialIndexSig {
            method: "gist_geometry_ops_2d",
            capabilities_json: "",
        };
        let b = SpatialIndexSig {
            method: "gist_geometry_ops_2d",
            capabilities_json: "{}",
        };
        assert_eq!(spatial_index_signature_hash(&a), spatial_index_signature_hash(&b));
    }

    #[test]
    fn spatial_index_hash_reflects_method() {
        let a = SpatialIndexSig {
            method: "gist_geometry_ops_2d",
            capabilities_json: "{}",
        };
        let b = SpatialIndexSig {
            method: "spgist_geometry_ops_2d",
            capabilities_json: "{}",
        };
        assert_ne!(spatial_index_signature_hash(&a), spatial_index_signature_hash(&b));
    }

    #[test]
    fn preprocessor_pattern_hash_reflects_function() {
        let a = PreprocessorPatternSig {
            op_token: "<->",
            function_name: "st_distance",
        };
        let b = PreprocessorPatternSig {
            op_token: "<->",
            function_name: "st_maxdistance",
        };
        assert_ne!(
            preprocessor_pattern_signature_hash(&a),
            preprocessor_pattern_signature_hash(&b)
        );
    }
}
