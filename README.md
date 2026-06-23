# shim-interface-core

Library: extract a DataFission wasm shim's SQL surface into a
SQLite database.

This is a generic engine. It walks any `.wasm` shim that
implements `datafission:df-plugin-api/extension@1.0.0` and
writes its scalar / aggregate / table function / window function
/ column type / system catalog / spatial index / cast / operator /
preprocessor surface to SQLite.

Per-shim binaries (`postgis-shim-interface`,
`mobilitydb-shim-interface`) are thin drivers that call into
this library and ship shim-specific starter queries.

## API

```rust
use std::path::Path;
use shim_interface_core::{open_fresh, extract_shim, print_summary};

let conn = open_fresh(Path::new("out.sqlite"))?;
let summary = extract_shim(&conn, Path::new("path/to/shim.wasm"))?;
println!("Extracted {} v{}", summary.name, summary.version);
print_summary(&conn)?;
```

The returned `SharedConn` is `Rc<RefCell<Connection>>`. Reuse
it across multiple `extract_shim` calls to put several shims
in one database (useful for `diff_snapshots` queries).

## Schema

`src/schema.sql`, embedded via `include_str!`. See per-shim
READMEs for table descriptions.

## Path dependencies

This crate path-deps into `../datafission/crates/{df-plugin-api,
df-plugin-loader, functions, index}` because parsing a
DataFission-WIT shim requires the DataFission loader. The
output `.sqlite` is fully portable; consumers
(`sqlink`/`ducklink`) read it without ever pulling in
DataFission.
