import { describe, expect, it } from "vitest";
import { replicaUnavailableReason, startWritableReplica } from "../src/replica/start";
import type { ProtocolBridge } from "../src/wasm/bridge";

const bridge = {} as ProtocolBridge;

const SAFARI_15_WASM_CSP =
  "Refused to create a WebAssembly object because 'unsafe-eval' is not an allowed source of script in the following Content Security Policy directive: \"script-src 'self' 'wasm-unsafe-eval'\".";

describe("replicaUnavailableReason", () => {
  it("classifies WASM CSP blocks without dumping the raw policy in the UI", () => {
    expect(replicaUnavailableReason(new EvalError(SAFARI_15_WASM_CSP))).toBe(
      "Writing unavailable: WebAssembly is blocked by this browser's Content Security Policy.",
    );
  });

  it("classifies IndexedDB open/close failures", () => {
    expect(replicaUnavailableReason(new DOMException("The database connection is closing.", "InvalidStateError"))).toBe(
      "Writing unavailable: browser storage could not be opened.",
    );
  });

  it("keeps other init exceptions in the user-facing reason", () => {
    expect(replicaUnavailableReason(new Error("WebAssembly.instantiate(): expected magic word"))).toBe(
      "Writing unavailable: WebAssembly.instantiate(): expected magic word",
    );
    expect(replicaUnavailableReason(undefined)).toBe("Writing unavailable: this browser could not start a writable replica.");
  });
});

describe("startWritableReplica", () => {
  it("does not open IndexedDB when loadBridge fails", async () => {
    let opened = 0;
    await expect(
      startWritableReplica({
        loadBridge: async () => {
          throw new EvalError(SAFARI_15_WASM_CSP);
        },
        openStore: async () => {
          opened += 1;
          return { close() {} };
        },
        live: () => true,
      }),
    ).rejects.toThrow(/unsafe-eval/);
    expect(opened).toBe(0);
  });

  it("closes a store opened after unmount between bridge and IDB ready", async () => {
    let live = true;
    let closed = 0;
    const out = await startWritableReplica({
      loadBridge: async () => {
        live = false;
        return bridge;
      },
      openStore: async () => ({
        close() {
          closed += 1;
        },
      }),
      live: () => live,
    });
    expect(out).toBeNull();
    expect(closed).toBe(0);

    live = true;
    const closedDuringOpen = await startWritableReplica({
      loadBridge: async () => bridge,
      openStore: async () => {
        live = false;
        return {
          close() {
            closed += 1;
          },
        };
      },
      live: () => live,
    });
    expect(closedDuringOpen).toBeNull();
    expect(closed).toBe(1);
  });
});
