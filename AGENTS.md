# Agent guide — shim-interface-core

This crate is the EXTRACTION-side library. Its job is to read a
composed DataFission shim `.wasm` and dump everything it
advertises into a SQLite database.

## Read this first

See `~/git/shim-bridge-codegen-core/PIPELINE.md` for the
six-repo map. This crate sits in the "extraction" layer —
upstream of `BridgePlan`/codegen.

## What lives here

- `src/schema.sql` — the canonical SQLite schema for the
  interface database. **This is the contract.** Per-shim
  extractors and downstream `BridgePlan` loaders both depend
  on it; changing a column shape ripples to both.
- `src/lib.rs` — `SqliteExtensionTarget` (an `ExtensionTarget`
  impl that records every `register_*` call to SQLite),
  `open_fresh` (creates a connection + applies schema), and
  `extract_shim` (drives one wasm shim's registry into the DB).

## How shims are queried

`extract_shim` instantiates a `RuntimeWasmExtension` via
`df-plugin-loader`, then:

1. Calls `ext.register(&mut SqliteExtensionTarget)` — the
   shim's `register_all_hooks` fans out per-capability
   callbacks (scalar/aggregate/table/window/data_type/index/
   system_catalog), each of which inserts one row.
2. Calls `ext.extract_sql_metadata()` to drain the
   sql-extension WIT metadata block (casts, operators,
   preprocessor patterns) — those don't flow through
   `ExtensionTarget` because the shim publishes them as a
   single `DynSqlExtension` globally.

## Common workflows

### Add a column to the schema

1. Add to `src/schema.sql`. Use the same key naming convention
   `(extension, name)` for new per-function tables.
2. Decide whether older interface DBs need to interop. If yes,
   add the column as nullable.
3. Insert the new value in the matching `register_*` callback
   in `src/lib.rs`.
4. Propagate downstream: add the field to
   `~/git/shim-bridge-codegen-core/src/plan.rs` and load it in
   `src/load.rs`. Codegen consumers pick it up automatically
   if it doesn't change existing field names.

### A function is missing from the extracted DB

1. Confirm the shim is actually registering it.
   `grep -n "register_scalar_function" extensions/<shim>/src/`
   in datafission should turn up the call site.
2. If the shim registers it but it's not in the DB, the
   `SqliteExtensionTarget` callback is failing silently
   (we use `let _ =` to swallow insert errors so one bad row
   doesn't tank the whole extraction). Add a `.context(...)`
   to see the cause.

## Things NOT to do

- **Don't bake target-database assumptions into the schema.**
  This layer is shim-shaped, not SQLite/DuckDB-shaped. If
  GEOMETRY is fundamentally a blob in some target and a real
  type in another, that's the codegen layer's problem.
- **Don't `wac plug` here.** The wasm artifact is an input;
  the loader path is in `df-plugin-loader`.

## Reference points

- `shim-bridge-codegen-core/PIPELINE.md` — the six-repo map.
- `~/git/datafission/crates/df-plugin-api/src/extension.rs` —
  the `ExtensionTarget` trait this crate implements.
- `~/git/datafission/crates/df-plugin-loader/src/wasm_extension.rs` —
  the loader that exposes `extract_sql_metadata` for the
  out-of-band metadata block.
