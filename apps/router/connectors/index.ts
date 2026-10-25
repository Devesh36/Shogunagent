/**
 * The connector registry: every provider the catalog compiles. Adding a connector means adding
 * its provider here (connectors/schema.md §6). The Monid passthrough is imported from the
 * fixture catalog JSON; the hosted router will refresh that JSON from Monid's catalog API.
 */
import { importMonidCatalog, type MonidCatalog } from "../src/passthrough/monid.js";
import type { ProviderDefinition } from "./types.js";
import monidFixture from "./monid/catalog.fixture.json" with { type: "json" };
import { shogunagentProvider } from "./shogunagent/provider.js";

export function allProviders(): ProviderDefinition[] {
  return [shogunagentProvider, importMonidCatalog(monidFixture as unknown as MonidCatalog)];
}
