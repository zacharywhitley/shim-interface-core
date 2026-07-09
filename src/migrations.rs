//! Forward-only schema migrations for the shim-interface DB.
//!
//! `open_fresh` is the greenfield path -- it wipes the file and
//! runs the whole of `schema.sql`, which already emits v2. The
//! migration codepath here exists so callers holding a persistent
//! v1 DB (the backfill script, downstream tooling with a
//! long-lived cache) can upgrade in place without a wholesale
//! re-extraction.

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

/// Apply migrations from `from` up to `to`. `from` is the value
/// currently written to `PRAGMA user_version` on the DB; `to` is
/// always [`crate::version::SCHEMA_VERSION`].
pub fn apply(conn: &Connection, from: u32, to: u32) -> Result<()> {
    if from > to {
        bail!("cannot migrate downwards ({from} -> {to})");
    }
    let mut v = from;
    while v < to {
        match v {
            // v0 and v1 are identical on-disk (v0 shipped before
            // we tagged `user_version`). Both upgrade the same way.
            0 | 1 => {
                apply_v1_to_v2(conn).context("applying v1 -> v2 migration")?;
                v = 2;
            }
            other => bail!(
                "no migration path from schema v{other}; \
                 update shim-interface-core"
            ),
        }
    }
    Ok(())
}

const V1_TO_V2_SQL: &str = include_str!("migrations/v1_to_v2.sql");

fn apply_v1_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(V1_TO_V2_SQL)
        .context("executing v1_to_v2.sql")?;
    Ok(())
}
