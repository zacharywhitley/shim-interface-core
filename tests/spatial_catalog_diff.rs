//! Integration tests for the `spatial-catalog-diff` binary.
//!
//! The binary is exercised as a subprocess so its `main`
//! (exit-code semantics, stderr/stdout separation, JSON emission)
//! is covered end-to-end, not just its module functions.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::params;
use serde_json::Value;
use shim_interface_core::open_fresh;

/// Absolute path to the built binary. Cargo sets
/// `CARGO_BIN_EXE_<name>` for every `[[bin]]` target in the crate
/// under test — no manual path juggling.
fn bin_path() -> PathBuf {
    env!("CARGO_BIN_EXE_spatial-catalog-diff").into()
}

/// Populate a tiny two-scalar DB and stamp a fake extension row.
fn seed_db(path: &Path, tweak_second_hash: bool, add_extra: bool, remove_first: bool) {
    let handle = open_fresh(path).expect("open_fresh");
    let conn = handle.borrow();
    conn.execute(
        "INSERT INTO extensions \
         (name, version, api_version, wasm_path, wasm_blake3, extracted_at) \
         VALUES ('demo', '1.0.0', '1.0.0', '/dev/null', 'x', '2026-01-01T00:00:00Z')",
        [],
    )
    .expect("insert extension");
    if !remove_first {
        conn.execute(
            "INSERT INTO scalars \
             (extension, name, param_types_json, return_type, is_deterministic, propagates_null, signature_hash) \
             VALUES ('demo', 'st_alpha', '[[\"float64\"]]', 'float64', 1, 1, 'aaaa')",
            [],
        )
        .expect("insert alpha");
    }
    let beta_hash = if tweak_second_hash { "bbbb-TWEAKED" } else { "bbbb" };
    conn.execute(
        "INSERT INTO scalars \
         (extension, name, param_types_json, return_type, is_deterministic, propagates_null, signature_hash) \
         VALUES ('demo', 'st_beta', '[[\"float64\"]]', 'float64', 1, 1, ?1)",
        params![beta_hash],
    )
    .expect("insert beta");
    if add_extra {
        conn.execute(
            "INSERT INTO scalars \
             (extension, name, param_types_json, return_type, is_deterministic, propagates_null, signature_hash) \
             VALUES ('demo', 'st_gamma', '[[\"float64\"]]', 'float64', 1, 1, 'cccc')",
            [],
        )
        .expect("insert gamma");
    }
}

fn run_bin(args: &[&std::ffi::OsStr]) -> (i32, String, String) {
    let out = Command::new(bin_path())
        .args(args)
        .output()
        .expect("spawn spatial-catalog-diff");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn additive_and_signature_change_yields_minor_with_changes() {
    let dir = tempfile::tempdir().unwrap();
    let before = dir.path().join("before.sqlite");
    let after = dir.path().join("after.sqlite");
    seed_db(&before, false, false, false);
    seed_db(&after, true, true, false); // tweak st_beta hash + add st_gamma

    let (code, stdout, _) = run_bin(&[
        "--before".as_ref(),
        before.as_os_str(),
        "--after".as_ref(),
        after.as_os_str(),
        "--output-format".as_ref(),
        "json".as_ref(),
    ]);
    assert_eq!(code, 0, "expected zero exit for MINOR-WITH-CHANGES");

    let v: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let scalars = v["families"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["family"] == "scalars")
        .expect("scalars family in report");
    assert_eq!(scalars["added"].as_array().unwrap().len(), 1);
    assert_eq!(scalars["removed"].as_array().unwrap().len(), 0);
    assert_eq!(scalars["signature_changed"].as_array().unwrap().len(), 1);
    assert_eq!(scalars["unchanged"], 1);
    assert_eq!(v["semver"]["class"], "minor_with_changes");
}

#[test]
fn removal_yields_major_and_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    let before = dir.path().join("before.sqlite");
    let after = dir.path().join("after.sqlite");
    seed_db(&before, false, false, false);
    seed_db(&after, false, false, true); // st_alpha removed

    let (code, _stdout, _stderr) = run_bin(&[
        "--before".as_ref(),
        before.as_os_str(),
        "--after".as_ref(),
        after.as_os_str(),
        "--output-format".as_ref(),
        "json".as_ref(),
    ]);
    assert_ne!(code, 0, "MAJOR must exit non-zero");
}

#[test]
fn entity_filter_narrows_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let before = dir.path().join("before.sqlite");
    let after = dir.path().join("after.sqlite");
    seed_db(&before, false, false, false);
    seed_db(&after, true, false, false); // tweak second hash only

    let (code, stdout, _) = run_bin(&[
        "--before".as_ref(),
        before.as_os_str(),
        "--after".as_ref(),
        after.as_os_str(),
        "--output-format".as_ref(),
        "json".as_ref(),
        "--entity-filter".as_ref(),
        "scalars".as_ref(),
    ]);
    assert_eq!(code, 0);
    let v: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let fams = v["families"].as_array().unwrap();
    assert_eq!(fams.len(), 1);
    assert_eq!(fams[0]["family"], "scalars");
}

#[test]
fn unknown_entity_filter_errors_out() {
    let dir = tempfile::tempdir().unwrap();
    let before = dir.path().join("before.sqlite");
    let after = dir.path().join("after.sqlite");
    seed_db(&before, false, false, false);
    seed_db(&after, false, false, false);

    let (code, _stdout, stderr) = run_bin(&[
        "--before".as_ref(),
        before.as_os_str(),
        "--after".as_ref(),
        after.as_os_str(),
        "--entity-filter".as_ref(),
        "scalars,nonsense".as_ref(),
    ]);
    assert_ne!(code, 0, "unknown filter must fail");
    assert!(
        stderr.contains("nonsense"),
        "stderr should mention the bad slug: {stderr}"
    );
}
