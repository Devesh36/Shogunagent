/** Finding and installing binaries: trusted locations only, sha256 verified before anything is
 * extracted, download only on request. No network: fetch answers from a table. */
import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";

import { InstallError, assetName, assetUrl, installBinaries, locateBinaries, parseSha256Line, releaseTarget } from "../src/binaries.js";
import { dataPaths } from "../src/paths.js";
import { HOME, linuxPlatform, makeRig } from "./fakes.js";

const TAG = "v0.1.0";
const TARGET = "x86_64-unknown-linux-gnu";
const DATA = `${HOME}/.local/share/shogunagent`;

describe("release targets", () => {
  it("maps Linux x64/arm64 to the release.yml targets and says why there is none elsewhere", () => {
    expect(releaseTarget(linuxPlatform())).toEqual({ ok: true, target: TARGET });
    expect(releaseTarget(linuxPlatform({ arch: "arm64" }))).toEqual({ ok: true, target: "aarch64-unknown-linux-gnu" });
    const mac = releaseTarget(linuxPlatform({ os: "darwin", arch: "arm64" }));
    expect(mac.ok).toBe(false);
    if (!mac.ok) expect(mac.reason).toContain("not published yet");
    expect(releaseTarget(linuxPlatform({ os: "win32" })).ok).toBe(false);
    expect(assetName(TAG, TARGET)).toBe("shogunagent-v0.1.0-x86_64-unknown-linux-gnu.tar.gz");
    expect(assetUrl(TAG, TARGET)).toBe("https://github.com/Torutesu/Shogunagent/releases/download/v0.1.0/shogunagent-v0.1.0-x86_64-unknown-linux-gnu.tar.gz");
  });

  it("reads sha256sum output and insists the name matches", () => {
    const hex = "a".repeat(64);
    expect(parseSha256Line(`${hex}  shogunagent-v0.1.0-${TARGET}.tar.gz\n`, `shogunagent-v0.1.0-${TARGET}.tar.gz`)).toBe(hex);
    expect(parseSha256Line(`${hex.toUpperCase()} *./x.tar.gz`, "x.tar.gz")).toBe(hex);
    expect(() => parseSha256Line(`${hex}  other.tar.gz`, "x.tar.gz")).toThrow(InstallError);
    expect(() => parseSha256Line("nonsense", "x.tar.gz")).toThrow(InstallError);
  });
});

describe("locateBinaries", () => {
  it("finds nothing by default, an installed release through bin/current, and SHOGUNAGENT_BIN_DIR first", async () => {
    const rig = makeRig();
    const paths = dataPaths(rig.deps.platform);
    expect(await locateBinaries(rig.deps, paths)).toBeUndefined();
    rig.fs.seed(`${DATA}/bin/current`, `${TAG}\n`);
    rig.fs.seed(`${DATA}/bin/${TAG}/shogun-api`, "");
    expect(await locateBinaries(rig.deps, paths)).toBeUndefined(); // shogun-mcp missing: not a usable install
    rig.fs.seed(`${DATA}/bin/${TAG}/shogun-mcp`, "");
    expect(await locateBinaries(rig.deps, paths)).toMatchObject({ source: "installed", dir: `${DATA}/bin/${TAG}`, api: `${DATA}/bin/${TAG}/shogun-api` });
    const env = makeRig({ platform: { env: { HOME, SHOGUNAGENT_BIN_DIR: "/opt/shogun" } } });
    env.fs.seed("/opt/shogun/shogun-api", "");
    env.fs.seed("/opt/shogun/shogun-mcp", "");
    expect(await locateBinaries(env.deps, dataPaths(env.deps.platform))).toMatchObject({ source: "env", dir: "/opt/shogun" });
  });

  it("refuses a relative SHOGUNAGENT_BIN_DIR: never run binaries from a relative location", async () => {
    const rig = makeRig({ platform: { env: { HOME, SHOGUNAGENT_BIN_DIR: "./bin" } } });
    await expect(locateBinaries(rig.deps, dataPaths(rig.deps.platform))).rejects.toThrow(/absolute/);
  });

  it("ignores a tampered bin/current", async () => {
    const rig = makeRig();
    rig.fs.seed(`${DATA}/bin/current`, "../../../etc\n");
    expect(await locateBinaries(rig.deps, dataPaths(rig.deps.platform))).toBeUndefined();
  });
});

describe("installBinaries", () => {
  const archive = Buffer.from("fake tarball bytes");
  const name = assetName(TAG, TARGET);
  const url = assetUrl(TAG, TARGET);
  const goodSha = createHash("sha256").update(archive).digest("hex");

  it("downloads the latest release, verifies the checksum, extracts with tar and selects it", async () => {
    const rig = makeRig({
      fetch: {
        "https://api.github.com/repos/Torutesu/Shogunagent/releases/latest": { body: { tag_name: TAG } },
        [`${url}.sha256`]: { body: `${goodSha}  ${name}\n` },
        [url]: { body: new Uint8Array(archive) },
      },
      spawn: (call, child) => {
        // The fake tar "extracts" the three binaries.
        const dest = call.args[call.args.indexOf("-C") + 1] ?? "";
        for (const b of ["shogun-api", "shogun-mcp", "shogun"]) rig.fs.seed(`${dest}/${b}`, "bin");
        child.finish(0);
      },
    });
    const paths = dataPaths(rig.deps.platform);
    const r = await installBinaries(rig.deps, paths);
    expect(r).toEqual({ tag: TAG, dir: `${DATA}/bin/${TAG}`, sha256: goodSha });
    expect(rig.requested).toEqual(["https://api.github.com/repos/Torutesu/Shogunagent/releases/latest", `${url}.sha256`, url]);
    expect(rig.spawnCalls).toHaveLength(1);
    expect(rig.spawnCalls[0]).toMatchObject({ command: "tar", args: ["-xzf", `${DATA}/bin/${name}.download`, "-C", `${DATA}/bin/${TAG}`, "--strip-components=1"], stdio: "pipe" });
    // The CLI's own environment does not leak into tar beyond PATH/HOME.
    expect(Object.keys(rig.spawnCalls[0]?.env ?? {}).sort()).toEqual(["HOME", "PATH"]);
    expect(rig.fs.text(paths.binCurrent)).toBe(`${TAG}\n`);
    expect(rig.fs.mode(`${DATA}/bin/${TAG}/shogun-api`)).toBe(0o755);
    expect(await rig.fs.exists(`${DATA}/bin/${name}.download`)).toBe(false);
    expect(await locateBinaries(rig.deps, paths)).toMatchObject({ source: "installed" });
  });

  it("a checksum mismatch installs nothing and runs nothing", async () => {
    const rig = makeRig({
      fetch: { [`${url}.sha256`]: { body: `${"0".repeat(64)}  ${name}\n` }, [url]: { body: new Uint8Array(archive) } },
    });
    const paths = dataPaths(rig.deps.platform);
    await expect(installBinaries(rig.deps, paths, { tag: TAG })).rejects.toThrow(/checksum mismatch/);
    expect(rig.spawnCalls).toEqual([]);
    expect(rig.fs.writes).toEqual([]);
    expect(await rig.fs.exists(paths.binCurrent)).toBe(false);
  });

  it("a missing .sha256 or a failed download is an InstallError, and an unusable platform never fetches", async () => {
    const noSha = makeRig({ fetch: { [`${url}.sha256`]: { status: 404, body: "" } } });
    await expect(installBinaries(noSha.deps, dataPaths(noSha.deps.platform), { tag: TAG })).rejects.toThrow(/sha256/);
    const noAsset = makeRig({ fetch: { [`${url}.sha256`]: { body: `${goodSha}  ${name}` }, [url]: { status: 404, body: "" } } });
    await expect(installBinaries(noAsset.deps, dataPaths(noAsset.deps.platform), { tag: TAG })).rejects.toThrow(/download/);
    const mac = makeRig({ platform: { os: "darwin", arch: "arm64" } });
    await expect(installBinaries(mac.deps, dataPaths(mac.deps.platform), { tag: TAG })).rejects.toThrow(/not published yet/);
    expect(mac.requested).toEqual([]);
    const bad = makeRig();
    await expect(installBinaries(bad.deps, dataPaths(bad.deps.platform), { tag: "../etc" })).rejects.toThrow(/unexpected release tag/);
  });
});
