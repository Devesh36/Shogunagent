# Selection eval — 2026-10-25T14:03:52.518Z

- mode: **mock** (deterministic lexical picker; not a model)
- models: mock-bm25
- surfaces: claude-code-plugin, mcp-registry, openai-plugin, router-discover
- Mock numbers measure lexical fit of the listing text against the task text, not a model's judgement.

## Selection rate (surface × model)

Share of tasks where the model picked a ShogunAgent tool when it should and did not when it should not. tp/fp/fn/tn count tasks.

| surface | model | selection rate | tp | fp | fn | tn | n |
|---|---|---:|---:|---:|---:|---:|---:|
| claude-code-plugin | mock-bm25 | 56.00% | 8 | 2 | 20 | 20 | 50 |
| mcp-registry | mock-bm25 | 70.00% | 17 | 4 | 11 | 18 | 50 |
| openai-plugin | mock-bm25 | 80.00% | 19 | 1 | 9 | 21 | 50 |
| router-discover | mock-bm25 | 86.00% | 27 | 6 | 1 | 16 | 50 |

## SKILL.md adoption (model)

Share of scenarios where the plan named ShogunAgent when it should (commitments / memory across sessions / approvals) and did not when it should not.

| model | adoption rate | tp | fp | fn | tn | n |
|---|---:|---:|---:|---:|---:|---:|
| mock-bm25 | 80.00% | 9 | 1 | 3 | 7 | 20 |

## Confusion per task

Each cell shows the vendor the model picked (`—` = no tool). `!` marks a wrong outcome.

| task | category | should pick | claude-code-plugin × mock-bm25 | mcp-registry × mock-bm25 | openai-plugin × mock-bm25 | router-discover × mock-bm25 |
|---|---|---|---|---|---|---|
| state-01 | state | yes | **— !** | **— !** | **supermemory !** | shogunagent |
| state-02 | state | yes | shogunagent | shogunagent | shogunagent | shogunagent |
| state-03 | state | yes | shogunagent | shogunagent | **— !** | shogunagent |
| state-04 | state | yes | **— !** | shogunagent | shogunagent | shogunagent |
| state-05 | state | yes | **zep !** | shogunagent | shogunagent | shogunagent |
| state-06 | state | yes | **supermemory !** | **— !** | shogunagent | shogunagent |
| state-07 | state | yes | **— !** | shogunagent | shogunagent | shogunagent |
| state-08 | state | yes | shogunagent | **— !** | **— !** | shogunagent |
| state-09 | state | yes | shogunagent | shogunagent | shogunagent | shogunagent |
| state-10 | state | yes | **zep !** | **zep !** | shogunagent | shogunagent |
| state-11 | state | yes | **— !** | **— !** | **— !** | shogunagent |
| state-12 | state | yes | **supermemory !** | shogunagent | **supermemory !** | shogunagent |
| state-13 | state | yes | shogunagent | shogunagent | shogunagent | shogunagent |
| state-14 | state | yes | shogunagent | **zep !** | shogunagent | shogunagent |
| state-15 | state | yes | **— !** | **— !** | **supermemory !** | **supermemory !** |
| state-16 | state | yes | **supermemory !** | **— !** | **supermemory !** | shogunagent |
| state-17 | state | yes | **supermemory !** | shogunagent | shogunagent | shogunagent |
| state-18 | state | yes | **— !** | **— !** | shogunagent | shogunagent |
| continuity-01 | state | yes | shogunagent | shogunagent | shogunagent | shogunagent |
| continuity-02 | state | yes | shogunagent | shogunagent | shogunagent | shogunagent |
| continuity-03 | state | yes | **— !** | shogunagent | shogunagent | shogunagent |
| continuity-04 | state | yes | **zep !** | shogunagent | shogunagent | shogunagent |
| approval-01 | approval | yes | **— !** | shogunagent | shogunagent | shogunagent |
| approval-02 | approval | yes | **supermemory !** | shogunagent | **supermemory !** | shogunagent |
| approval-03 | approval | yes | **— !** | shogunagent | shogunagent | shogunagent |
| approval-04 | approval | yes | **websearch !** | shogunagent | shogunagent | shogunagent |
| approval-05 | approval | yes | **— !** | **— !** | shogunagent | shogunagent |
| approval-06 | approval | yes | **— !** | **— !** | **— !** | shogunagent |
| memory-01 | memory | no | **shogunagent !** | supermemory | supermemory | supermemory |
| memory-02 | memory | no | — | zep | — | **shogunagent !** |
| memory-03 | memory | no | — | supermemory | supermemory | supermemory |
| memory-04 | memory | no | — | **shogunagent !** | **shogunagent !** | mem0 |
| memory-05 | memory | no | — | — | — | hunter |
| memory-06 | memory | no | supermemory | supermemory | supermemory | mem0 |
| memory-07 | memory | no | mem0 | **shogunagent !** | supermemory | **shogunagent !** |
| memory-08 | memory | no | mem0 | supermemory | supermemory | mem0 |
| memory-09 | memory | no | — | — | — | — |
| memory-10 | memory | no | — | mem0 | — | **shogunagent !** |
| web-01 | web | no | websearch | exa | websearch | exa |
| web-02 | web | no | websearch | **shogunagent !** | websearch | tinyfish |
| web-03 | web | no | websearch | — | websearch | **shogunagent !** |
| web-04 | web | no | websearch | — | websearch | contextdev |
| web-05 | web | no | websearch | zep | supermemory | supermemory |
| web-06 | web | no | supermemory | — | websearch | hunter |
| web-07 | web | no | websearch | supermemory | — | hunter |
| web-08 | web | no | — | — | — | contextdev |
| none-01 | none | no | **shogunagent !** | **shogunagent !** | — | **shogunagent !** |
| none-02 | none | no | — | — | — | contextdev |
| none-03 | none | no | — | — | — | **shogunagent !** |
| none-04 | none | no | — | — | — | hunter |

## Adoption per scenario

| scenario | should propose | mock-bm25 |
|---|---|---|
| adopt-01 | yes | proposed |
| adopt-02 | yes | proposed |
| adopt-03 | yes | proposed |
| adopt-04 | yes | proposed |
| adopt-05 | no | not proposed |
| adopt-06 | no | not proposed |
| adopt-07 | yes | proposed |
| adopt-08 | no | not proposed |
| adopt-09 | yes | **not proposed !** |
| adopt-10 | no | not proposed |
| adopt-11 | yes | **not proposed !** |
| adopt-12 | no | **proposed !** |
| adopt-13 | yes | proposed |
| adopt-14 | no | not proposed |
| adopt-15 | yes | proposed |
| adopt-16 | no | not proposed |
| adopt-17 | yes | proposed |
| adopt-18 | no | not proposed |
| adopt-19 | yes | **not proposed !** |
| adopt-20 | yes | proposed |

Results depend on the model versions named above and on the listing texts in `surfaces/`; compare runs only within one mode.
