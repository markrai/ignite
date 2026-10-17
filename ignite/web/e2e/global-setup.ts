import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { NATIVE_DIR, WEB_DIST, serverBin } from "./harness";

/** Build the real sync server once; require a production web build (`npm run build`). */
export default function globalSetup(): void {
  if (!process.env.SCRIBE_SYNC_SERVER_BIN) {
    execFileSync("cargo", ["build", "-q", "-p", "scribe-sync-server", "-p", "scribe-server"], { cwd: NATIVE_DIR, stdio: "inherit" });
  }
  if (!existsSync(serverBin())) throw new Error(`sync server binary missing: ${serverBin()}`);
  if (!existsSync(join(WEB_DIST, "index.html"))) {
    throw new Error("ignite/web/dist is missing: run `npm run build` before `npm run test:e2e`");
  }
}
