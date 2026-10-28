# Website and fresh-user verification

Verified locally on 28 October 2026. This records observed behavior; it does not certify a production deployment or the complete engine feature set.

## Website

- All 18 content pages and the 404 document have valid local destinations: 574 links and fragment references checked.
- All 44 generated pages/assets requested over HTTP returned successfully.
- The three automated website tests passed: navigation/assets, quickstart safeguards, and HTTP routes/content negotiation/404/405 handling.
- Browser checks covered the product and documentation routes, docs filtering and empty results, setup prompt copying, Human/Agent view, FAQ expansion, product tabs, MacBook chapters, relationship graph, timeline update/reset, and the homepage context-handoff animation.
- All three playground scenarios reached simulated approval. Empty source selection disables progression; reset, sample JSON rendering, and JSON download worked.
- At a 390px viewport, mobile navigation opened, navigated to Get started, and closed. No horizontal overflow was observed.
- Browser automation intermittently timed out; affected interactions were rechecked. A download-event wait timed out, but the generated JSON file was verified on disk. Observed browser console errors originated in the installed MetaMask extension.

Run the website checks from the repository root:

```sh
npm run typecheck --prefix apps/web
npm test --prefix apps/web
```

The test command builds its own static output, so it also works in a fresh checkout. The website CI job runs these same checks without installing additional dependencies.

## Fresh local engine setup

The local landing page's quickstart was followed using a fresh public clone of `Torutesu/Shogunagent` at `4e8210fdc16fbd13a04d7b94d7dfbe8fbce4a246`, a new build directory, and a new evaluation store. Existing Rust/compiler dependency caches were available. The macOS toolchain was Rust/Cargo stable 1.97.1, x86_64.

| Check | Observed result |
| --- | --- |
| Exact release build from the guide | Passed in 16m 46s, without source changes or compiler workarounds |
| Private evaluation directory | Directory mode 0700, key/token modes 0600 |
| Repeat initialization | Refused the existing directory; both original keys remained unchanged |
| Server startup | Listening on `127.0.0.1:7464` in multi-principal mode |
| First ingest | Accepted 1, inserted 1, event ID 1 |
| Recall | Returned `The Kestrel demo codeword is violet.` as source evidence |
| Restart with the same key/store | Returned the same event and excerpt |
| Missing/wrong bearer token | HTTP 401 |
| Cross-principal recall without a grant | HTTP 403 |
| Repeated idempotency key | `replayed: true`, `inserted: 0`, same event ID |
| Unknown query | Empty evidence |
| HTTP MCP with the required headers | Initialization, 68-tool listing, `profile.whoami`, and `memory.recall` succeeded |
| Handoff creation/acceptance | HTTP 200; accepted the task into `agent_scratch` |

The execution sandbox initially prohibited binding/accessing localhost; the authorized local tests outside that sandbox succeeded. This was an execution-environment restriction, not an engine failure. Low disk space was resolved by removing only the explicitly approved, regenerable build cache before compiling.

## Known gaps for follow-up

1. **Publication:** The copied setup prompt points to `shogunagent.com`. The new `/docs/quickstart/` route returned HTTP 404 on the public domain during verification; the local preview serves it. Publish the complete docs alongside the homepage, or supply a usable preview-specific entry point.
2. **Client setup:** The MCP page names the endpoint and bearer header but does not explain required protocol/header behavior. A request containing only those documented headers returned HTTP 400 with `missing_header: Mcp-Method`. Adding `Mcp-Method`, plus `Mcp-Name` for `tools/call`, succeeded. Exact, tested client-specific instructions are still needed. Direct protocol tests do not establish that Claude Code, Codex, Cursor, or Claude Desktop have been connected successfully.
3. **Illustration versus configured behavior:** This quickstart configures lexical retrieval, not model processing or automatic extraction. Recall returned empty `facts` and `items`; handoff creation returned empty `facts` and `entities`. The task-only handoff succeeded, but the rich Maya workflow and relationship graph are browser simulations. A tested extraction/model walkthrough remains necessary.
4. **Engine onboarding copy:** The MCP initialization instructions still describe “ShogunAI” and screen/meeting memory on a Mac. That wording is confusing for the separate ShogunAgent server product.

Actual client integration, model extraction, live provider calls, connector OAuth, external sends, hosted deployment/billing, Windows, and the complete Rust workspace test suite were not validated in this website audit. No external messages or model calls were made, and no deployment was performed.

## Visual review

These screenshots show illustrative browser UI, not a live connection to the engine.

![MacBook workflow](docs/preview/macbook-demo.jpg)

![Get Started section](docs/preview/get-started.jpg)
