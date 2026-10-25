// Offline replay test in Monid's convention (`deno task test` replays fixtures/*.json without a
// network). Helper names follow DEVELOPMENT.md as summarized in our research notes; align them with
// connectors/exa/endpoints/*/endpoint.test.ts before the PR (CHECKLIST item 1).
import { replayFixtures } from "../../../../engine/testing.ts";

import provider from "../../provider.ts";
import { stateCommitmentsList } from "./endpoint.ts";

replayFixtures(provider, stateCommitmentsList, new URL("./fixtures/", import.meta.url), {
  // The caller's key is a synthetic value; recorded fixtures never contain headers.
  params: { shogunagentKey: "sa_test_synthetic_key" },
});
