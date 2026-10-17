// Production build invariant: npm run build must install WASM from the current
// Cargo workspace and fail closed — never silently reuse a stale checked-in module.

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import {
  copyWasmArtifact,
  findEmittedWasmAssets,
  prepareBrowserWasm,
  resolveCargoTargetDir,
  resolveCargoWasmArtifact,
  resolveRepoRoot,
  runCargoWasmBuild,
  sha256File,
  verifyDistWasmMatches,
} from "../scripts/wasm-build.mjs";

const webDir = join(fileURLToPath(new URL(".", import.meta.url)), "..");
const mockCargo = join(webDir, "tests", "helpers", "mock-cargo.mjs");
const productionScripts = [
  join(webDir, "scripts", "wasm-build.mjs"),
  join(webDir, "scripts", "build-wasm.mjs"),
  join(webDir, "scripts", "build-production.mjs"),
  join(webDir, "scripts", "verify-dist-wasm.mjs"),
];

/** @type {string[]} */
const temps: string[] = [];

function tempDir(prefix: string): string {
  const d = mkdtempSync(join(tmpdir(), prefix));
  temps.push(d);
  return d;
}

afterEach(() => {
  while (temps.length) {
    const d = temps.pop();
    if (d) rmSync(d, { recursive: true, force: true });
  }
});

/** Prepend a directory containing a failing `cargo` shim to PATH. */
function envWithFailingCargo(baseEnv: NodeJS.ProcessEnv, targetDir: string): NodeJS.ProcessEnv {
  const binDir = join(tempDir("ignite-fake-cargo-"), "bin");
  mkdirSync(binDir, { recursive: true });
  if (process.platform === "win32") {
    writeFileSync(
      join(binDir, "cargo.cmd"),
      "@echo simulated cargo failure>&2\r\n@exit /b 1\r\n",
    );
  } else {
    const sh = join(binDir, "cargo");
    writeFileSync(sh, "#!/bin/sh\necho simulated cargo failure >&2\nexit 1\n");
    chmodSync(sh, 0o755);
  }
  const prev = baseEnv.PATH ?? baseEnv.Path ?? "";
  return {
    ...baseEnv,
    CARGO_TARGET_DIR: targetDir,
    PATH: `${binDir}${delimiter}${prev}`,
    Path: `${binDir}${delimiter}${prev}`,
  };
}

describe("production WASM path resolution", () => {
  it("resolves the Cargo workspace from ignite/web", () => {
    const root = resolveRepoRoot(webDir);
    expect(existsSync(join(root, "Cargo.toml"))).toBe(true);
    expect(readFileSync(join(root, "Cargo.toml"), "utf8")).toContain("scribe-web-wasm");
  });

  it("honors CARGO_TARGET_DIR including paths with spaces", () => {
    const spaced = join(tempDir("ignite-wasm-td-"), "target dir with spaces");
    mkdirSync(spaced, { recursive: true });
    const resolved = resolveCargoTargetDir({ CARGO_TARGET_DIR: spaced });
    expect(resolved).toBe(spaced);
    const artifact = resolveCargoWasmArtifact(resolveRepoRoot(webDir), resolved);
    expect(artifact).toBe(
      join(spaced, "wasm32-unknown-unknown", "release", "scribe_web_wasm.wasm"),
    );
  });

  it("defaults Cargo artifact under <repo>/target when CARGO_TARGET_DIR unset", () => {
    const root = resolveRepoRoot(webDir);
    const artifact = resolveCargoWasmArtifact(root, null);
    expect(artifact).toBe(
      join(root, "target", "wasm32-unknown-unknown", "release", "scribe_web_wasm.wasm"),
    );
  });
});

describe("dist WASM verification", () => {
  it("accepts a single fingerprinted asset that matches Cargo bytes", () => {
    const dir = tempDir("ignite-wasm-dist-");
    const cargo = join(dir, "cargo.wasm");
    const dist = join(dir, "dist");
    const assets = join(dist, "assets");
    mkdirSync(assets, { recursive: true });
    const body = Buffer.from("\0asm\x01\x00\x00\x00fresh-bytes");
    writeFileSync(cargo, body);
    writeFileSync(join(assets, "scribe_web_wasm-AbCdEf12.wasm"), body);
    const v = verifyDistWasmMatches(cargo, dist);
    expect(v.sha256).toBe(createHash("sha256").update(body).digest("hex"));
  });

  it("fails closed when dist WASM is missing, ambiguous, or mismatched", () => {
    const dir = tempDir("ignite-wasm-bad-");
    const cargo = join(dir, "cargo.wasm");
    writeFileSync(cargo, Buffer.from("alpha"));
    const emptyDist = join(dir, "empty");
    mkdirSync(emptyDist);
    expect(() => verifyDistWasmMatches(cargo, emptyDist)).toThrow(/no \.wasm/);

    const multi = join(dir, "multi");
    mkdirSync(join(multi, "assets"), { recursive: true });
    writeFileSync(join(multi, "assets", "a.wasm"), Buffer.from("alpha"));
    writeFileSync(join(multi, "assets", "b.wasm"), Buffer.from("alpha"));
    expect(() => verifyDistWasmMatches(cargo, multi)).toThrow(/ambiguous/);

    const mismatch = join(dir, "mismatch");
    mkdirSync(join(mismatch, "assets"), { recursive: true });
    writeFileSync(join(mismatch, "assets", "x.wasm"), Buffer.from("beta"));
    expect(() => verifyDistWasmMatches(cargo, mismatch)).toThrow(/SHA-256 mismatch/);
  });

  it("findEmittedWasmAssets walks nested asset dirs", () => {
    const dist = join(tempDir("ignite-wasm-walk-"), "dist");
    mkdirSync(join(dist, "assets"), { recursive: true });
    writeFileSync(join(dist, "assets", "scribe_web_wasm-x.wasm"), Buffer.from("z"));
    expect(findEmittedWasmAssets(dist)).toHaveLength(1);
  });
});

describe("prepareBrowserWasm fail-closed freshness", () => {
  it("refreshes an intentionally stale browser WASM from Cargo output", () => {
    const root = tempDir("ignite-wasm-fresh-");
    const targetDir = join(root, "target dir with spaces");
    const browserWasm = join(root, "src", "wasm", "scribe_web_wasm.wasm");
    mkdirSync(dirname(browserWasm), { recursive: true });
    writeFileSync(browserWasm, Buffer.from("STALE_PLACEHOLDER_DO_NOT_SHIP"));
    const staleHash = sha256File(browserWasm);

    const prepared = prepareBrowserWasm({
      webDir,
      browserWasmPath: browserWasm,
      targetDir,
      cargoRunner: { command: process.execPath, argsPrefix: [mockCargo] },
      env: {
        ...process.env,
        CARGO_TARGET_DIR: targetDir,
        MOCK_CARGO_PAYLOAD: "fresh-from-cargo",
      },
    });

    expect(prepared.sha256).not.toBe(staleHash);
    expect(sha256File(browserWasm)).toBe(prepared.sha256);
    expect(sha256File(prepared.cargoArtifact)).toBe(prepared.sha256);
    expect(readFileSync(browserWasm)).toEqual(
      Buffer.from("\0asm\x01\x00\x00\x00fresh-from-cargo"),
    );
  });

  it("does not overwrite the browser WASM when Cargo fails", () => {
    const root = tempDir("ignite-wasm-fail-");
    const targetDir = join(root, "target");
    const browserWasm = join(root, "out", "scribe_web_wasm.wasm");
    mkdirSync(dirname(browserWasm), { recursive: true });
    const prior = Buffer.from("KEEP_PRIOR_ON_FAILURE");
    writeFileSync(browserWasm, prior);

    expect(() =>
      prepareBrowserWasm({
        webDir,
        browserWasmPath: browserWasm,
        targetDir,
        cargoRunner: { command: process.execPath, argsPrefix: [mockCargo] },
        env: {
          ...process.env,
          CARGO_TARGET_DIR: targetDir,
          MOCK_CARGO_FAIL: "1",
        },
      }),
    ).toThrow(/Cargo build of scribe-web-wasm failed/);

    expect(readFileSync(browserWasm)).toEqual(prior);
    expect(existsSync(resolveCargoWasmArtifact(resolveRepoRoot(webDir), targetDir))).toBe(
      false,
    );
  });

  it("copyWasmArtifact rejects hash mismatch destinations", () => {
    const dir = tempDir("ignite-wasm-copy-");
    const from = join(dir, "from.wasm");
    const to = join(dir, "sub", "to.wasm");
    writeFileSync(from, Buffer.from("copy-me"));
    const hash = copyWasmArtifact(from, to);
    expect(sha256File(to)).toBe(hash);
  });
});

describe("production Cargo provenance (no env override)", () => {
  it("production scripts do not reference IGNITE_WEB_CARGO or mock-cargo", () => {
    expect(existsSync(join(webDir, "scripts", "mock-cargo.mjs"))).toBe(false);
    expect(existsSync(mockCargo)).toBe(true);
    for (const file of productionScripts) {
      const src = readFileSync(file, "utf8");
      expect(src, file).not.toMatch(/IGNITE_WEB_CARGO/);
      expect(src, file).not.toMatch(/mock-cargo/);
    }
  });

  it("ignores IGNITE_WEB_CARGO and never installs mock-cargo bytes", () => {
    const root = tempDir("ignite-wasm-no-env-");
    const targetDir = join(root, "target");
    const browserWasm = join(root, "out", "scribe_web_wasm.wasm");
    mkdirSync(dirname(browserWasm), { recursive: true });
    const prior = Buffer.from("PRIOR_UNTOUCHED");
    writeFileSync(browserWasm, prior);

    // Even with a successful mock pointed at by the removed override, production
    // resolution must call real `cargo` (here: failing shim on PATH).
    expect(() =>
      prepareBrowserWasm({
        webDir,
        browserWasmPath: browserWasm,
        targetDir,
        env: envWithFailingCargo(
          {
            ...process.env,
            IGNITE_WEB_CARGO: mockCargo,
            MOCK_CARGO_PAYLOAD: "EVIL_MOCK_MUST_NOT_APPEAR",
            MOCK_CARGO_FAIL: "0",
          },
          targetDir,
        ),
      }),
    ).toThrow(/Cargo build of scribe-web-wasm failed/);

    expect(readFileSync(browserWasm)).toEqual(prior);
    expect(readFileSync(browserWasm).toString()).not.toContain("EVIL_MOCK");
  });

  it("runCargoWasmBuild without cargoRunner does not honor IGNITE_WEB_CARGO", () => {
    const targetDir = join(tempDir("ignite-wasm-runcargo-"), "target");
    expect(() =>
      runCargoWasmBuild({
        repoRoot: resolveRepoRoot(webDir),
        targetDir,
        env: envWithFailingCargo(
          {
            ...process.env,
            IGNITE_WEB_CARGO: mockCargo,
            MOCK_CARGO_PAYLOAD: "SHOULD_NOT_WRITE",
          },
          targetDir,
        ),
      }),
    ).toThrow(/Cargo build of scribe-web-wasm failed/);
    expect(existsSync(resolveCargoWasmArtifact(resolveRepoRoot(webDir), targetDir))).toBe(
      false,
    );
  });
});

describe("production build script wiring", () => {
  it("package.json build invokes the Node production orchestrator", () => {
    const pkg = JSON.parse(readFileSync(join(webDir, "package.json"), "utf8")) as {
      scripts: Record<string, string>;
    };
    expect(pkg.scripts.build).toContain("build-production.mjs");
    expect(pkg.scripts["build:wasm"]).toContain("build-wasm.mjs");
    expect(pkg.scripts.build).not.toMatch(/^tsc /);
  });

  it("repo build_web_wasm.sh delegates to the Node script", () => {
    const sh = readFileSync(
      join(resolveRepoRoot(webDir), "tools", "build_web_wasm.sh"),
      "utf8",
    );
    expect(sh).toContain("ignite/web/scripts/build-wasm.mjs");
    expect(sh).toContain("exec node");
  });

  it("build-production.mjs fails closed when Cargo fails (no Vite success)", () => {
    const realWasm = join(webDir, "src", "wasm", "scribe_web_wasm.wasm");
    const before = existsSync(realWasm) ? readFileSync(realWasm) : null;
    const targetDir = join(tempDir("ignite-wasm-prod-fail-"), "target");
    const result = spawnSync(
      process.execPath,
      [join(webDir, "scripts", "build-production.mjs")],
      {
        cwd: webDir,
        env: envWithFailingCargo(
          {
            ...process.env,
            // Must be ignored if someone still exports it.
            IGNITE_WEB_CARGO: mockCargo,
            MOCK_CARGO_PAYLOAD: "EVIL_FROM_ENV",
          },
          targetDir,
        ),
        encoding: "utf8",
        shell: false,
      },
    );
    expect(result.status).not.toBe(0);
    const out = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
    expect(out).toMatch(/Cargo build of scribe-web-wasm failed/);
    expect(out).not.toMatch(/production build OK/);
    expect(out).not.toMatch(/mock-cargo/);
    expect(out).not.toMatch(/EVIL_FROM_ENV/);
    if (before) {
      expect(readFileSync(realWasm)).toEqual(before);
    }
  });
});

/** Isolate a tiny fake dist that mirrors Vite fingerprinting for end-to-end hash check. */
describe("cargo→browser→dist hash chain (fixture)", () => {
  it("dist verification matches the Cargo module after prepare+copy", () => {
    const root = tempDir("ignite-wasm-chain-");
    const targetDir = join(root, "cargo-target");
    const browserWasm = join(root, "browser", "scribe_web_wasm.wasm");
    const dist = join(root, "dist", "assets");
    mkdirSync(dist, { recursive: true });

    const prepared = prepareBrowserWasm({
      webDir,
      browserWasmPath: browserWasm,
      targetDir,
      cargoRunner: { command: process.execPath, argsPrefix: [mockCargo] },
      env: {
        ...process.env,
        CARGO_TARGET_DIR: targetDir,
        MOCK_CARGO_PAYLOAD: "chain-v1",
      },
    });

    cpSync(browserWasm, join(dist, "scribe_web_wasm-Fingerprint1.wasm"));
    const verified = verifyDistWasmMatches(prepared.cargoArtifact, join(root, "dist"));
    expect(verified.sha256).toBe(prepared.sha256);
  });
});
