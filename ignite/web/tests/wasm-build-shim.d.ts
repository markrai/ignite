declare module "*wasm-build.mjs" {
  export function resolveRepoRoot(webDir: string): string;
  export function resolveCargoTargetDir(env?: NodeJS.ProcessEnv): string | null;
  export function resolveCargoWasmArtifact(
    repoRoot: string,
    targetDir: string | null,
  ): string;
  export function resolveBrowserWasmPath(webDir: string): string;
  export function sha256File(filePath: string): string;
  export function findEmittedWasmAssets(distDir: string): string[];
  export function verifyDistWasmMatches(
    cargoWasmPath: string,
    distDir: string,
  ): { path: string; sha256: string };
  export function runCargoWasmBuild(opts: {
    repoRoot: string;
    cargoBin?: string;
    cargoRunner?: { command: string; argsPrefix?: string[] };
    targetDir?: string | null;
    env?: NodeJS.ProcessEnv;
  }): {
    artifact: string;
    targetDir: string | null | undefined;
    sha256: string;
    size: number;
  };
  export function copyWasmArtifact(from: string, to: string): string;
  export function prepareBrowserWasm(opts: {
    webDir: string;
    browserWasmPath?: string;
    cargoBin?: string;
    cargoRunner?: { command: string; argsPrefix?: string[] };
    targetDir?: string | null;
    env?: NodeJS.ProcessEnv;
    runCargo?: typeof runCargoWasmBuild;
  }): {
    repoRoot: string;
    cargoArtifact: string;
    browserWasm: string;
    sha256: string;
    size: number;
  };
}
