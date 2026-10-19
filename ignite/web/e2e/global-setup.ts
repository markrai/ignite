import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { NATIVE_DIR, WEB_DIST, serverBin } from "./harness";
import { exportBin } from "./product";

/** Build the real servers once; require a production web build (`npm run build`). */
export default function globalSetup(): void {
  if (!process.env.SCRIBE_SYNC_SERVER_BIN) {
    execFileSync("cargo", ["build", "-q", "-p", "scribe-sync-server", "-p", "scribe-server", "-p", "backup-export"], { cwd: NATIVE_DIR, stdio: "inherit" });
  } else if (!process.env.IGNITE_BACKUP_EXPORT_BIN && !existsSync(exportBin())) {
    // Custom server binaries: build only the exporter the restore tests need, and only when it
    // is missing. A failure here affects those tests alone (they report the missing binary).
    try {
      execFileSync("cargo", ["build", "-q", "-p", "backup-export"], { cwd: NATIVE_DIR, stdio: "inherit" });
    } catch (e) {
      console.warn(`ignite-backup-export could not be built (${String(e)}); the restore admin tests will fail.`);
    }
  }
  if (!existsSync(serverBin())) throw new Error(`sync server binary missing: ${serverBin()}`);
  if (!existsSync(join(WEB_DIST, "index.html"))) {
    throw new Error("ignite/web/dist is missing: run `npm run build` before `npm run test:e2e`");
  }
}
