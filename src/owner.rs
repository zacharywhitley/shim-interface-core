//! Owner-file resolution: given a shim's WIT interface name
//! (e.g. `"postgis-measurements"`), return the Rust file under
//! `src/` that implements it. Each shim CLI owns its own map.
//!
//! [`SourceMetadata`] is the bundle passed to [`crate::extract_shim`]
//! when the caller wants hashes and dependency edges computed
//! alongside the SQL surface. `None` keeps legacy behaviour.

use std::path::{Path, PathBuf};

/// Owner-file resolver -- pluggable per shim.
pub trait OwnerResolver: Send + Sync {
    /// Return the absolute Rust source path that implements
    /// `interface`, or `None` if unmapped.
    fn owner_file(&self, interface: &str) -> Option<PathBuf>;

    /// Enumerate every interface this resolver knows about. Used
    /// by the backfill script to walk the WIT set.
    fn known_interfaces(&self) -> Vec<String>;
}

/// A resolver that maps interface names via a fixed table plus a
/// prefix classifier. `src_root` is prepended to each entry to
/// yield the absolute path.
pub struct StaticOwnerResolver {
    pub src_root: PathBuf,
    /// (interface, relative-path) pairs. `interface` is
    /// case-sensitive and matches the WIT name exactly.
    pub entries: Vec<(&'static str, &'static str)>,
}
impl OwnerResolver for StaticOwnerResolver {
    fn owner_file(&self, interface: &str) -> Option<PathBuf> {
        self.entries
            .iter()
            .find(|(k, _)| *k == interface)
            .map(|(_, rel)| self.src_root.join(rel))
    }
    fn known_interfaces(&self) -> Vec<String> {
        self.entries.iter().map(|(k, _)| k.to_string()).collect()
    }
}

/// Source-tree metadata bundled onto [`crate::extract_shim`] when the
/// caller wants hashes / edges filled in.
pub struct SourceMetadata<'a> {
    /// Root of the shim's Rust source tree (`~/git/postgis-wasm/src`).
    pub src_root: &'a Path,
    /// Directory holding the shim's helpers -- typically
    /// `<src_root>/helpers`. Contents are folded into every
    /// implementation hash.
    pub helpers_root: &'a Path,
    /// Upstream release identifier (e.g. `"3.4.2"`). Written to
    /// `first_seen_upstream_version` on new rows and
    /// `last_seen_upstream_version` on every touched row.
    pub upstream_version: &'a str,
    /// Git SHA of the shim source tree, if known. Stored on the
    /// upstream_versions row for provenance.
    pub upstream_commit: Option<&'a str>,
    /// Optional upstream release timestamp (RFC3339).
    pub released_at: Option<&'a str>,
    /// Owner-file resolver.
    pub owner_map: &'a dyn OwnerResolver,
    /// When `true`, skip the syn-based source walk (only run the
    /// SQL-derived edge queries). Handy in CI where source may be
    /// absent.
    pub skip_source_walk: bool,
}
