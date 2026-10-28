# Website setup prompt: real Codex acceptance test

## Outcome — 28 October 2026

**The corrected onboarding and shared-context workflow passed in Codex CLI
0.164.0, with a separate engine terminal and a client restart.** It is not a
zero-interaction installer. The original prompt exposed lifecycle, identity,
and unnecessary-write problems; those were corrected and retested.

[Actual prompts, thread IDs, tool calls, source results, and answers](verification/setup-prompt-acceptance-2026-10-28.json)
are preserved as synthetic evidence. The four accepted work sessions made
11 successful MCP calls. No expected answers were supplied to the receiving
sessions, and no MCP server definition was injected into their command lines.

## What was exercised

1. Created a blank Git project with one existing `AGENTS.md` convention and no
   Shogun MCP configuration, keys, or store. Passed the actual prompt downloaded
   from `http://localhost:3000/setup-prompt.txt` to a fresh Codex session.
2. Supplied only evaluation boundaries and available local resources: preserve
   global settings and other stores, use this project, reuse the existing source
   checkout and release binary because only about 1.3 GB was free, and use port
   7466. No bridge configuration was prepared for the agent.
3. Codex fetched the local references, created private keys and a dedicated
   principal store, wrote the start script, generated project-local MCP
   configuration, preserved the original convention, and installed memory
   instructions. The protected configuration-directory write went through the
   client's approval mechanism.
4. After the first failures below, reran the revised prompt against this project.
   It preserved the key/token bytes and MCP entry, corrected the actor metadata,
   avoided duplicate instruction sections, and provided an honest manual handoff.
5. Followed that handoff by running the generated start script in a separate
   terminal, then launched fresh Codex sessions from the project directory.
   They loaded the on-disk configuration written by the setup agent.
6. Supplied an ordinary project decision, without naming Shogun or telling the
   agent to save it. Codex followed `AGENTS.md`, recalled context, and ingested
   the decision as event 1.
7. Asked a fresh session for the project's owner, date, format, random phrase,
   and unrecorded budget. It recovered the source. Its extra write exposed the
   read-only policy problem below. Codex then installed the corrected instruction
   block from the updated website prompt in a targeted repair.
8. Repeated the fresh read, supplied an explicit deadline revision, and asked
   another fresh session for the current plan. Both final reads used only read
   tools; the real revision was saved as event 3.

## Problems found and changes made

| Problem observed | Correction | Evidence after correction |
| --- | --- | --- |
| The engine started inside Codex's temporary command session stopped when that session ended. The subsequent bridge check failed at initialization. | Prompt and connection guide explain that the engine must outlive a client session, prefer a dedicated terminal, and require an explicit pending step when that terminal must be started by the user. | The revised setup explicitly stopped its temporary server and handed off the start command. The separate engine stayed available across all subsequent Codex sessions. |
| Generated instructions tried to discover the authenticated actor through `profile.whoami`. That reads the saved user profile, which can be empty. | Prompt and guide specify `agent:<agent_id>` from the local bearer mapping, with `agent:<principal>` as the no-agent-ID fallback. They explicitly distinguish profile data from authentication. | Codex corrected its instructions to `agent:codex-project`; actual writes with that actor succeeded. |
| A read-only status question triggered a new world event summarizing the retrieved answer, despite explicitly stating that no decisions changed. | Shared memory instructions now forbid ingesting read-only answers, "verified" copies, or summaries of recall as new evidence. Real updates should reference earlier sources. | Both final read sessions omitted `memory.ingest`. A genuine revision still invoked it successfully. The old diagnostic duplicate remains event 2 in the isolated store, rather than being deleted to hide the failure. |
| The preview server cached the prompt-generating module, so rebuilding the website could update the Copy button while the full-prompt endpoint still served an earlier version. | The endpoint now reads the current built text and rewrites only the preview origin. | A regression test changes the built prompt while the server is running and verifies that the endpoint immediately serves the change. |

The public setup prompt remains agent-neutral. Codex-specific configurations and
instructions were produced by the agent for this particular client, not hard-coded
as the only supported product in the website prompt.

## Accepted workflow results

The synthetic project was `Cedar-713816e1`.

| Session | Actual result |
| --- | --- |
| Decision | Recalled an empty baseline, then saved Rina Das / 2030-08-14 / HTML and a random verification phrase as event 1. The user message asked only for acknowledgment of a project decision. |
| Fresh read after policy correction | Retrieved the owner, date, format, and phrase; opened source events; reported budget unknown. No ingest. It identified diagnostic event 2 as a reference to event 1, not independent confirmation. |
| Revision | Recalled and opened the previous source, then saved the explicit change to 2030-08-22 as event 3, preserving other decisions. |
| Fresh revised read | Selected 2030-08-22 over 2030-08-14, cited events 1 and 3, retained the other values, and kept budget unknown. No ingest. |

The fresh-read prompts contained only the project label and requested field
names, not the owner, dates, format value, or random phrase. Those answers were
also absent from `AGENTS.md`, `README.md`, `SHOGUN-SETUP.md`, and the MCP config.
Each accepted session had a different thread ID and did not resume or fork a
previous conversation. Built-in memories, apps, and plugins were disabled for
the test; shell tools were disabled during the four ordinary work sessions.

## Harness and cleanup details

Some diagnostic invocations failed to load the project configuration. The
configuration-isolation flag was unsuitable for testing an installed project;
successful runs used the actual project working directory, loaded its generated
configuration, and disabled unrelated global MCP servers only for those
invocations. The earlier controlled bridge tests injected a configuration; this
acceptance test deliberately did not. A missing server was reported honestly as
unavailable, with no successful save claimed.

The CLI added a temporary project-trust entry to the user's configuration during
setup. After all sessions ended, the configuration was restored byte-for-byte to
its recorded baseline, checking for intervening changes before restoration.
The dedicated port-7466 server was stopped. The original demo server, preview,
existing stores, and private test store were preserved. No settings were pushed
or deployed. Raw local transcripts and invocation manifests are under
`/private/tmp/shogun-prompt-acceptance-20261028/`; no credentials are included in
the repository evidence artifact.

## Scope and remaining limits

- This exercised the reusable-engine path, not a new compilation on a clean
  machine. The prebuilt binary was the previously audited local release; it was
  not rebuilt for this task, so newer initialization wording is not covered.
- There was an explicit separate-terminal start and client restart between setup
  and actual tool use. The prompt must not promise that a newly registered MCP
  server appears automatically in an already-running conversation.
- The final no-reingest rule was installed through a targeted Codex repair and
  tested with real reads and a real revision; a second completely blank-machine
  installation of that final prompt was not run.
- This proves source-based context sharing in Codex CLI. It does not establish
  Claude/Cursor/Desktop behavior, Batch extraction, typed handoffs, semantic
  conflict resolution, whole-chat capture, or universal instruction adherence.
- Syntax checks, all four website tests, and six bridge tests passed. A complete
  Rust rebuild, workspace test matrix, and production deployment were not part
  of this prompt/documentation change.

To repeat the user flow: open an empty test project, paste the local website
prompt, follow its engine-terminal and client-restart steps, then supply a
synthetic decision. Start a fresh conversation with only the project label and
ask for its current details and source evidence. Repeat after changing one
detail. Inspect actual tool calls; a status lookup should not create an event.
