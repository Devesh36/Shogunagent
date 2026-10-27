import { describe, expect, it } from "vitest";
import { main } from "../src/cli.js";
import { HOME, makeRig, type Rig } from "./fakes.js";

const DATA = `${HOME}/.local/share/shogunagent`;
const KEY = "ab".repeat(32);
const TOKEN = "synthetic-private-token";
function prepared(): Rig {
  const rig = makeRig();
  rig.fs.seed(`${DATA}/bin/current`, "v0.1.0");
  for (const binary of ["shogun-api", "shogun-mcp"]) rig.fs.seed(`${DATA}/bin/v0.1.0/${binary}`, "binary");
  rig.fs.seed(`${DATA}/memory.db`, "synthetic encrypted store placeholder");
  rig.fs.seed(`${DATA}/db.key`, KEY);
  rig.fs.seed(`${DATA}/api.token`, TOKEN);
  rig.fs.seed(`${DATA}/memory_api.json`, '{"enabled":true}');
  return rig;
}
async function run(rig: Rig) {
  const before = [...rig.fs.files];
  const dirs = [...rig.fs.dirs];
  const code = await main(["doctor", "--json"], rig.deps);
  expect(rig.fs.writes).toEqual([]);
  expect([...rig.fs.files]).toEqual(before);
  expect([...rig.fs.dirs]).toEqual(dirs);
  expect(rig.requested).toEqual([]);
  expect(rig.spawnCalls).toEqual([]);
  const text = [...rig.out, ...rig.err].join("\n");
  expect(text).not.toContain(KEY);
  expect(text).not.toContain(TOKEN);
  return { code, report: JSON.parse(rig.out.join("\n")) as { ok: boolean; checks: Array<{ id: string; status: string; message: string }>; limitations: string[] } };
}
function check(result: Awaited<ReturnType<typeof run>>, id: string) {
  return result.report.checks.find((c) => c.id === id);
}
describe("doctor", () => {
  it.each([["22.12.0", "fail"], ["20.19.0", "fail"], ["22.13.0", "pass"], ["22.14.0", "pass"], ["24.0.0", "pass"]])("checks Node minimum for %s", async (version, status) => {
    const rig = prepared();
    rig.deps.platform.nodeVersion = version;
    expect(check(await run(rig), "node")?.status).toBe(status);
  });
  it("renders actionable plain text as well as JSON", async () => {
    const rig = makeRig();
    expect(await main(["doctor"], rig.deps)).toBe(1);
    expect(rig.out.join("\n")).toContain("FAIL store:");
    expect(rig.out.join("\n")).toContain("SHOGUNAGENT_DATA_DIR");
    expect(rig.fs.writes).toEqual([]);
  });
  it("treats blank environment overrides as absent and falls back to files", async () => {
    const rig = prepared();
    rig.deps.platform.env.SHOGUN_DB_KEY = "  ";
    rig.deps.platform.env.SHOGUN_API_TOKEN = "\n";
    expect((await run(rig)).code).toBe(0);
  });
  it("reports all fresh-install blockers without creating a directory, key or settings", async () => {
    const result = await run(makeRig());
    expect(result.code).toBe(1);
    for (const id of ["binaries", "store", "db_key"]) expect(check(result, id)?.status).toBe("fail");
    for (const id of ["api_token", "memory_api"]) expect(check(result, id)?.status).toBe("warn");
  });
  it("accepts present prerequisites but explicitly does not certify decryption or liveness", async () => {
    const result = await run(prepared());
    expect(result.code).toBe(0);
    expect(result.report.ok).toBe(true);
    expect(result.report.limitations.join(" ")).toMatch(/decryption.*schema/i);
    expect(result.report.limitations.join(" ")).toMatch(/liveness/i);
  });
  it("uses environment secrets ahead of invalid files, just as serve does", async () => {
    const rig = prepared();
    rig.deps.platform.env.SHOGUN_DB_KEY = ` ${KEY} `;
    rig.deps.platform.env.SHOGUN_API_TOKEN = TOKEN;
    rig.fs.seed(`${DATA}/db.key`, "invalid");
    rig.fs.seed(`${DATA}/api.token`, "");
    const result = await run(rig);
    expect(result.code).toBe(0);
    expect(check(result, "db_key")?.message).toContain("environment");
  });
  it("rejects an invalid environment key without falling back to a valid file or echoing it", async () => {
    const rig = prepared();
    rig.deps.platform.env.SHOGUN_DB_KEY = TOKEN;
    const result = await run(rig);
    expect(result.code).toBe(1);
    expect(check(result, "db_key")?.status).toBe("fail");
  });
  it.each(['{"enabled":false}', '{"enabled":"true"}', '{}', `{"enabled":true, "private":"${TOKEN}"`])("rejects disabled or malformed settings without printing contents: case %#", async (settings) => {
    const rig = prepared();
    rig.fs.seed(`${DATA}/memory_api.json`, settings);
    expect(check(await run(rig), "memory_api")?.status).toBe("fail");
  });
  it.each(['{"enabled":true,"profile":null}', '{"enabled":true,"auto_connect":"true"}'])("does not certify additional settings fields: case %#", async (settings) => {
    const rig = prepared();
    rig.fs.seed(`${DATA}/memory_api.json`, settings);
    const result = await run(rig);
    expect(check(result, "memory_api")?.status).toBe("warn");
    expect(check(result, "memory_api")?.message).toContain("not validated");
  });
  it("warns about generated token/settings without blocking an otherwise prepared store", async () => {
    const rig = prepared();
    await rig.fs.remove(`${DATA}/api.token`);
    await rig.fs.remove(`${DATA}/memory_api.json`);
    const result = await run(rig);
    expect(result.code).toBe(0);
    expect(check(result, "api_token")?.status).toBe("warn");
    expect(check(result, "memory_api")?.status).toBe("warn");
  });
  it("reports unreadable secret files safely and continues other checks", async () => {
    const rig = prepared();
    const original = rig.fs.readFile.bind(rig.fs);
    rig.fs.readFile = (path) => path.endsWith("db.key") ? Promise.reject(new Error(TOKEN)) : original(path);
    const result = await run(rig);
    expect(result.code).toBe(1);
    expect(check(result, "db_key")?.status).toBe("fail");
    expect(check(result, "memory_api")?.status).toBe("pass");
  });
  it("does not treat a directory as a store or binary", async () => {
    const rig = prepared();
    await rig.fs.remove(`${DATA}/memory.db`);
    rig.fs.mkdirSync(`${DATA}/memory.db`);
    await rig.fs.remove(`${DATA}/bin/v0.1.0/shogun-api`);
    rig.fs.mkdirSync(`${DATA}/bin/v0.1.0/shogun-api`);
    const result = await run(rig);
    expect(check(result, "store")?.status).toBe("fail");
    expect(check(result, "binaries")?.status).toBe("fail");
  });
  it("supports explicit source binaries and data directory on a platform without release downloads", async () => {
    const rig = makeRig({ platform: { os: "darwin", env: { SHOGUNAGENT_DATA_DIR: "/store", SHOGUNAGENT_BIN_DIR: "/source/release", SHOGUN_DB_KEY: KEY } } });
    rig.fs.seed("/source/release/shogun-api", "binary");
    rig.fs.seed("/source/release/shogun-mcp", "binary");
    rig.fs.seed("/store/memory.db", "synthetic");
    const result = await run(rig);
    expect(result.code).toBe(0);
    expect(check(result, "release_target")?.status).toBe("warn");
  });
  it("blocks relative data directories before serve can resolve nested store paths", async () => {
    const rig = prepared();
    rig.deps.platform.env.SHOGUNAGENT_DATA_DIR = TOKEN;
    const result = await run(rig);
    expect(result.code).toBe(1);
    expect(check(result, "data_dir")?.status).toBe("fail");
  });
  it("reports a relative binary override as a blocker without exposing its contents", async () => {
    const rig = prepared();
    rig.deps.platform.env.SHOGUNAGENT_BIN_DIR = TOKEN;
    expect(check(await run(rig), "binaries")?.status).toBe("fail");
  });
});
