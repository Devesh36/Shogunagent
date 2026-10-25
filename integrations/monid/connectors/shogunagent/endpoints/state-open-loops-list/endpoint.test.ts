// Offline replay test in Monid's convention; align helper names with connectors/exa/ before the PR.
import { replayFixtures } from "../../../../engine/testing.ts";

import provider from "../../provider.ts";
import { stateOpenLoopsList } from "./endpoint.ts";

replayFixtures(provider, stateOpenLoopsList, new URL("./fixtures/", import.meta.url), {
  params: { shogunagentKey: "sa_test_synthetic_key" },
});
