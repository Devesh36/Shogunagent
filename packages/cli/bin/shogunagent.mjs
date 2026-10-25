#!/usr/bin/env node
// `npx shogunagent` (plan E10-T10). The code is compiled from src/ into dist/ at publish time.
import { main } from "../dist/bin.js";

process.exitCode = await main(process.argv.slice(2));
