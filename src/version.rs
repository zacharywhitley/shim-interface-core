//! `PRAGMA user_version` discipline for the shim-interface DB.
//!
//! Forward-only. Every extractor invocation reads `user_version`
//! and refuses to touch a DB that's newer than [`SCHEMA_VERSION`].
//! A DB that's older is upgraded via [`crate::migrations::apply`]
//! before any writes go through.
//!
//! See `AGENTS.md` and B0 design doc for the schema shape at
//! each version.

use std::cmp::Ordering;

use anyhow::{bail, Result};
use rusqlite::Connection;

/// The schema version this build of `shim-interface-core` writes
/// and understands. Bump in lockstep with `schema.sql`'s
/// `PRAGMA user_version = N`.
pub const SCHEMA_VERSION: u32 = 3;

/// Read the DB's `PRAGMA user_version`. A fresh SQLite file
/// reports 0 -- that's treated as the pre-v1 legacy shape (which
/// is byte-identical to v1 anyway, since v0 never explicitly
/// tagged itself).
pub fn read_user_version(conn: &Connection) -> Result<u32> {
    let v: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(v)
}

/// Write `PRAGMA user_version`. Callers only need this from the
/// migration path -- `open_fresh` gets the tag via the trailing
/// `PRAGMA user_version = 2` in `schema.sql`.
pub fn write_user_version(conn: &Connection, v: u32) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {v}"))?;
    Ok(())
}

/// Verify the DB is at [`SCHEMA_VERSION`], upgrading in place if it's
/// older. Errors out if it's newer -- forward-only means the DB is
/// authoritative for what it holds; the extractor must be at least
/// as new as the DB.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    let v = read_user_version(conn)?;
    match v.cmp(&SCHEMA_VERSION) {
        Ordering::Equal => Ok(()),
        Ordering::Less => crate::migrations::apply(conn, v, SCHEMA_VERSION),
        Ordering::Greater => bail!(
            "DB schema v{v} is newer than tool schema v{SCHEMA_VERSION}; \
             upgrade shim-interface-core"
        ),
    }
}
