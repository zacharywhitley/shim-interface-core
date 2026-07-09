//! syn-based call-edge walker for shim source trees.
//!
//! Consumed by [`crate::extract_source_metadata`] to populate
//! `function_dependencies` with `call` / `call_method` / `macro` /
//! `indirect` edges. SQL-derived edges (`type_arg`, `type_return`,
//! `cast_target`, `operator_bind`) run as separate INSERT ... SELECT
//! statements against the pre-existing catalog columns.
//!
//! Design notes (mirrors partition_bridge_manifest.rs primitives at
//! `~/git/datafission/crates/df-plugin-loader/src/bin/`):
//!   - Traversal is `syn::visit::Visit` on every `Item::Impl` /
//!     `Item::Fn`; walker respects nested closures/async blocks by
//!     default.
//!   - Turbofish arguments are cleared before recording so
//!     `foo::<T>()` and `foo()` collapse to the same callee name.
//!   - `use helpers::*` glob expansion is resolved from a
//!     pre-scanned set of `pub fn` idents.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use syn::visit::Visit;
use syn::{
    Expr, ExprAsync, ExprCall, ExprClosure, ExprMacro, ExprMethodCall, ExprPath, ImplItemFn,
    ItemFn, ItemImpl,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    Call,
    CallMethod,
    Macro,
    Indirect,
}
impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Call => "call",
            EdgeKind::CallMethod => "call_method",
            EdgeKind::Macro => "macro",
            EdgeKind::Indirect => "indirect",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub callee_name: String,
    pub edge_kind: EdgeKind,
    pub source_hint: String,
}

#[derive(Debug, Clone)]
pub struct WalkedFn {
    pub caller_module: String,
    pub caller_name: String,
    pub edges: Vec<Edge>,
}

/// Walk every `.rs` file under `src_root`, returning one
/// [`WalkedFn`] per top-level function or impl-method. Files that
/// fail to parse are skipped with a diagnostic on stderr -- a bad
/// file shouldn't crash the whole extraction.
pub fn walk_shim_src(src_root: &Path) -> Result<Vec<WalkedFn>> {
    let helper_idents = scan_helper_idents(&src_root.join("helpers")).unwrap_or_default();
    let mut out = Vec::new();
    if !src_root.exists() {
        return Ok(out);
    }
    let entries: Vec<_> = walkdir::WalkDir::new(src_root)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension() == Some(OsStr::new("rs")))
        .collect();

    for entry in entries {
        let path = entry.path();
        // Skip the helpers subtree -- its idents are the target
        // of glob-imports, not walk-worthy call sources.
        if path
            .strip_prefix(src_root)
            .map(|p| p.starts_with("helpers"))
            .unwrap_or(false)
        {
            continue;
        }
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("walk_shim_src: skipping {}: {e}", path.display());
                continue;
            }
        };
        let file: syn::File = match syn::parse_file(&text) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("walk_shim_src: syn parse failed on {}: {e}", path.display());
                continue;
            }
        };
        let module = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        walk_items(&file.items, &module, &helper_idents, &mut out);
    }
    Ok(out)
}

fn walk_items(
    items: &[syn::Item],
    module: &str,
    helpers: &BTreeSet<String>,
    out: &mut Vec<WalkedFn>,
) {
    for item in items {
        match item {
            syn::Item::Impl(im) => visit_impl(im, module, helpers, out),
            syn::Item::Fn(fnd) => visit_free_fn(fnd, module, helpers, out),
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    // Preserve the top-level file stem as the module
                    // label -- nested `mod wasm { ... }` blocks (used
                    // by cfg-gated shim source trees like mobilitydb)
                    // should still be attributed to the file they
                    // live in.
                    walk_items(inner, module, helpers, out);
                }
            }
            _ => {}
        }
    }
}

fn visit_impl(
    im: &ItemImpl,
    module: &str,
    helpers: &BTreeSet<String>,
    out: &mut Vec<WalkedFn>,
) {
    for it in &im.items {
        if let syn::ImplItem::Fn(m) = it {
            visit_impl_fn(m, module, helpers, out);
        }
    }
}

fn visit_impl_fn(
    m: &ImplItemFn,
    module: &str,
    helpers: &BTreeSet<String>,
    out: &mut Vec<WalkedFn>,
) {
    let mut collector = EdgeCollector::new(helpers.clone());
    collector.visit_block(&m.block);
    if collector.edges.is_empty() {
        return;
    }
    out.push(WalkedFn {
        caller_module: module.to_string(),
        caller_name: m.sig.ident.to_string(),
        edges: collector.edges,
    });
}

fn visit_free_fn(
    fnd: &ItemFn,
    module: &str,
    helpers: &BTreeSet<String>,
    out: &mut Vec<WalkedFn>,
) {
    let mut collector = EdgeCollector::new(helpers.clone());
    collector.visit_block(&fnd.block);
    if collector.edges.is_empty() {
        return;
    }
    out.push(WalkedFn {
        caller_module: module.to_string(),
        caller_name: fnd.sig.ident.to_string(),
        edges: collector.edges,
    });
}

struct EdgeCollector {
    helpers: BTreeSet<String>,
    edges: Vec<Edge>,
    seen: BTreeSet<(String, EdgeKind, String)>,
}
impl EdgeCollector {
    fn new(helpers: BTreeSet<String>) -> Self {
        Self { helpers, edges: Vec::new(), seen: BTreeSet::new() }
    }
    fn push(&mut self, callee: String, kind: EdgeKind, hint: String) {
        let key = (callee.clone(), kind, hint.clone());
        if self.seen.insert(key) {
            self.edges.push(Edge {
                callee_name: callee,
                edge_kind: kind,
                source_hint: hint,
            });
        }
    }
}

impl<'ast> Visit<'ast> for EdgeCollector {
    fn visit_expr_call(&mut self, ec: &'ast ExprCall) {
        match &*ec.func {
            Expr::Path(ExprPath { path, .. }) => {
                let path_text = quote_path(path);
                let last = last_segment(path);
                if path_text.starts_with("Self::") {
                    self.push(last, EdgeKind::Call, path_text);
                } else if path.segments.len() == 1 {
                    // Bare identifier: could be a `use helpers::*`
                    // glob hit or a same-module free fn.
                    let ident = last;
                    if self.helpers.contains(&ident) {
                        self.push(
                            ident.clone(),
                            EdgeKind::Call,
                            format!("helpers::{ident}"),
                        );
                    } else {
                        self.push(ident.clone(), EdgeKind::Call, ident);
                    }
                } else {
                    self.push(last, EdgeKind::Call, path_text);
                }
            }
            _ => {
                // Indirect call: local var / expression producing
                // a fn. Best-effort textual hint.
                let hint = quote_expr(&ec.func);
                let name = hint.split_whitespace().next().unwrap_or("<indirect>").to_string();
                self.push(name, EdgeKind::Indirect, hint);
            }
        }
        syn::visit::visit_expr_call(self, ec);
    }

    fn visit_expr_method_call(&mut self, m: &'ast ExprMethodCall) {
        let method = m.method.to_string();
        let receiver = quote_expr(&m.receiver);
        let hint = format!("{receiver}.{method}");
        self.push(method, EdgeKind::CallMethod, hint);
        syn::visit::visit_expr_method_call(self, m);
    }

    fn visit_expr_macro(&mut self, em: &'ast ExprMacro) {
        let path_text = quote_path(&em.mac.path);
        let last = last_segment(&em.mac.path);
        self.push(last, EdgeKind::Macro, format!("{path_text}!"));
        syn::visit::visit_expr_macro(self, em);
    }

    fn visit_stmt_macro(&mut self, sm: &'ast syn::StmtMacro) {
        let path_text = quote_path(&sm.mac.path);
        let last = last_segment(&sm.mac.path);
        self.push(last, EdgeKind::Macro, format!("{path_text}!"));
        syn::visit::visit_stmt_macro(self, sm);
    }

    fn visit_expr_closure(&mut self, c: &'ast ExprClosure) {
        // syn::visit already recurses; explicitly noting the
        // no-op here for clarity.
        syn::visit::visit_expr_closure(self, c);
    }

    fn visit_expr_async(&mut self, a: &'ast ExprAsync) {
        syn::visit::visit_expr_async(self, a);
    }
}

fn quote_path(p: &syn::Path) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(p.segments.len());
    if p.leading_colon.is_some() {
        parts.push(String::new());
    }
    for seg in &p.segments {
        parts.push(seg.ident.to_string());
    }
    parts.join("::")
}

fn last_segment(p: &syn::Path) -> String {
    p.segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_else(|| "<empty>".to_string())
}

fn quote_expr(e: &Expr) -> String {
    // Cheap best-effort textual approximation. Avoids pulling
    // proc-macro2 into a public API contract.
    match e {
        Expr::Path(p) => quote_path(&p.path),
        Expr::Field(f) => format!("{}.{}",
            quote_expr(&f.base),
            match &f.member {
                syn::Member::Named(id) => id.to_string(),
                syn::Member::Unnamed(idx) => idx.index.to_string(),
            }),
        Expr::MethodCall(m) => format!("{}.{}", quote_expr(&m.receiver), m.method),
        Expr::Call(c) => quote_expr(&c.func),
        _ => "<expr>".to_string(),
    }
}

/// Collect every `pub fn` identifier under a helpers directory so
/// bare-ident calls that hit a helper resolve to `helpers::<name>`
/// hints. Missing directory -> empty set.
fn scan_helper_idents(root: &Path) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension() == Some(OsStr::new("rs")))
    {
        let text = fs::read_to_string(entry.path())
            .with_context(|| format!("reading {}", entry.path().display()))?;
        let file = match syn::parse_file(&text) {
            Ok(f) => f,
            Err(_) => continue,
        };
        collect_pub_fns(&file.items, &mut out);
    }
    Ok(out)
}

fn collect_pub_fns(items: &[syn::Item], out: &mut BTreeSet<String>) {
    for item in items {
        match item {
            syn::Item::Fn(fnd) => {
                if matches!(fnd.vis, syn::Visibility::Public(_)) {
                    out.insert(fnd.sig.ident.to_string());
                }
            }
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    collect_pub_fns(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// Convenience: absolute paths for the standard shim layout
/// `<crate>/src` + `<crate>/src/helpers`. Returned so CLIs can
/// short-circuit `SourceMetadata` construction.
pub fn standard_source_layout(shim_root: &Path) -> (PathBuf, PathBuf) {
    (shim_root.join("src"), shim_root.join("src").join("helpers"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn walk_synthetic_measurements() {
        let dir = tempdir();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("helpers")).unwrap();
        // Helper file exporting `parse_ewkb`.
        write_file(
            &src.join("helpers").join("wkb.rs"),
            "pub fn parse_ewkb() {}\n",
        );
        // Owner file mixing Self::, method call, macro, and a
        // helper call.
        write_file(
            &src.join("measurements.rs"),
            r#"
                struct PostgisImpl;
                impl PostgisImpl {
                    pub fn st_area(&self) {
                        let g = Self::st_perimeter();
                        let _v = g.unsigned_area();
                        parse_ewkb();
                        println!("hi");
                    }
                    pub fn st_perimeter() {}
                }
            "#,
        );
        let out = walk_shim_src(&src).unwrap();
        let st_area = out
            .iter()
            .find(|w| w.caller_name == "st_area")
            .expect("st_area");
        let names: BTreeSet<_> = st_area.edges.iter().map(|e| e.callee_name.clone()).collect();
        assert!(names.contains("st_perimeter"));
        assert!(names.contains("unsigned_area"));
        assert!(names.contains("parse_ewkb"));
        assert!(names.contains("println"));
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
    fn write_file(p: &Path, contents: &str) {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }
}
