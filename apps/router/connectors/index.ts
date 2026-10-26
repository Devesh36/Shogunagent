/**
 * The connector registry: every provider the catalog compiles. Adding a connector means adding
 * its provider here (connectors/schema.md §6). The Monid passthrough is imported from the
 * fixture catalog JSON; the hosted router will refresh that JSON from Monid's catalog API.
 */
import { importMonidCatalog, type MonidCatalog } from "../src/passthrough/monid.js";
import type { ProviderDefinition } from "./types.js";
import monidFixture from "./monid/catalog.fixture.json" with { type: "json" };
import { calendarProvider } from "./calendar/provider.js";
import { githubProvider } from "./github/provider.js";
import { gmailProvider } from "./gmail/provider.js";
import { linearProvider } from "./linear/provider.js";
import { notionProvider } from "./notion/provider.js";
import { shogunagentProvider } from "./shogunagent/provider.js";
import { slackProvider } from "./slack/provider.js";

/** First-party connectors (E8, E22, E51): ranked by the same score as everything else, labeled first_party. */
export function firstPartyProviders(): ProviderDefinition[] {
  return [shogunagentProvider, gmailProvider, calendarProvider, githubProvider, linearProvider, slackProvider, notionProvider];
}

export function allProviders(): ProviderDefinition[] {
  return [...firstPartyProviders(), importMonidCatalog(monidFixture as unknown as MonidCatalog)];
}
