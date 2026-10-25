# Open tool catalog

Every endpoint listed in the ShogunAgent router, public and without a key: price, live health, p50/p95 latency, categories, the first-party label, the permission level, the provider's data-handling statement, and the score formula `discover` ranks with. Plan: E29. Served by the router (`apps/router/src/open-catalog.ts`) from the compiled catalog — nothing on it is written by hand. Tests: `apps/router/test/open-catalog.test.ts`.

| Route | Returns |
|---|---|
| `GET /v1/catalog` | `{ format: "shogunagent-open-catalog/1", catalog_hash, entry_count, provider_count, ranking, pricing, entries[] }`. Filters: `?category=state`, `?provider=monid`, `?first_party=true` |
| `GET /v1/catalog/{tool_id}` | one entry plus `description`, `exclusions`, `notes`, `input_schema`, `output_schema` and the formula; `404 tool_not_found` with `next_action: "discover"` otherwise |
| `GET /catalog` | the same list as an HTML page: no JavaScript, every value escaped, `Content-Security-Policy: default-src 'none'` |

An entry:

```json
{
  "tool_id": "shogunagent.state.commitments.list", "provider_id": "shogunagent", "provider_name": "ShogunAgent",
  "first_party": true, "label": "first_party",
  "summary": "Find what the user committed to: promises, deliverables and deadlines they made to others.",
  "categories": ["state", "memory"],
  "permission": { "level": "L1", "class": "read", "meaning": "Runs immediately. Reads only; nothing leaves the user's data." },
  "price": { "model": "per_call", "unit": "call", "cost_micro_usd": 0, "quote_micro_usd": 0, "quote_usd": "$0", "provider_quote_micro_usd": 0, "take_bps": 0, "failures_cost": 0, "empty_results_cost": 0 },
  "health": { "p50_ms": 40, "p95_ms": 120, "success_rate": 0.999, "source": "declared" },
  "data_handling": { "retention": "user_account", "retention_days": 0, "zero_retention": false, "regions": ["global"], "statement": "…" },
  "docs_url": "https://…", "entry_hash": "…",
  "links": { "detail": "/v1/catalog/shogunagent.state.commitments.list", "inspect": "/v1/router/inspect", "run": "/v1/router/run" }
}
```

- `price`: the usage-model figures (`cost_micro_usd`, …) and `provider_quote_micro_usd` are the provider's own price (for a marketplace import such as Monid, the marketplace's price). `quote_micro_usd` is what the caller pays for a typical run: that price plus one take, `take_bps` (500 for third-party tools, 0 for first-party), charged once by the wallet (E32, `docs/api/payouts.md`).
- `health.source` is `observed` once the router has seen 20 runs of the tool, `declared` (the provider's baseline) before that.
- `data_handling` is `null` for passthrough entries (an imported marketplace catalog), which are handled under that marketplace's terms.
- `ranking.formula` is the exact formula `router.discover` uses; `ranking.policy` states that first-party tools get no boost, no pinned slot and no hidden term (CLAUDE.md invariant 13).
- Legal names, contacts and payout references from `provider.json` are never published: they are not in the compiled catalog at all.
