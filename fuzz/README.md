# Fuzz targets (plan E38)

libFuzzer targets for the parsers that take untrusted input. **Nightly only** (cargo-fuzz needs
`-Zsanitizer`); the crate is excluded from the main workspace, so stable CI never builds it. It
does type-check on stable (`cargo check --bins` in this directory).

| Target | Entry point | Invariants |
|---|---|---|
| `mcp_jsonrpc` | `McpServer::handle_line`, `mcp_http::handle` | no panic; replies are JSON; metering headers are sendable values; what ran is what was metered |
| `export_import` | `shogun_memory::export::import_v1` | no panic; no database error on malformed input; every record accounted for; no open transaction |

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run mcp_jsonrpc -- -max_total_time=300
cargo +nightly fuzz run export_import -- -max_total_time=300
```

The same properties run on stable in the normal test suite, seeded so a failure replays:

- `crates/shogun-mcp/src/jsonrpc_prop_tests.rs`
- `crates/shogun-memory/src/export/prop_tests.rs`
- `apps/api/test/saml-fuzz.test.ts` (the SAML Response parser, malformed XML; TypeScript, so it is
  a seeded mutation test rather than a libFuzzer target)

Findings so far are listed in `docs/security/threat-model.md` §7.
