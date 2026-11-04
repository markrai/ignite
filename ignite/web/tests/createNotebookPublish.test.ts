// Slice 6B: session/replica identity gate after deferred blob PUT; Blank fast path.

import { describe, expect, it, vi } from "vitest";
import type { BlobApi } from "../src/api/blobApi";
import {
  commitStructureOn,
  initiatingCreateStillValid,
  publishThenCreateNotebook,
  type ReadyReplica,
} from "../src/paper/createNotebookPublish";
import { publishTemplateBackground } from "../src/paper/publishTemplateBackground";
import * as paperTemplates from "../src/paper/paperTemplates";
import type { LibrarySession } from "../src/state/librarySession";
import type { SyncCoordinator } from "../src/replica/coordinator";

function session(tag: string): LibrarySession {
  return { libraryId: tag } as unknown as LibrarySession;
}

function readyReplica(blobApi: BlobApi, commits: unknown[]): ReadyReplica {
  let n = 0;
  return {
    kind: "ready",
    blobApi,
    mintId: (k) => `${k}-${++n}`,
    createNotebookRender: () =>
      ({
        profile_version: 2,
        ink_byte: 0,
        legacy_fixed_radius: 2,
        transform_id: "t",
        rasterizer_id: "r",
      }) as ReturnType<ReadyReplica["createNotebookRender"]>,
    coordinator: {
      commit: async (draft: unknown) => {
        commits.push(draft);
        return { envelope: "{}", operation_id: "op" };
      },
    } as unknown as SyncCoordinator,
  };
}

describe("initiatingCreateStillValid", () => {
  it("requires mounted + same replica object + same session object", () => {
    const blobApi: BlobApi = { put: vi.fn() };
    const initiating = readyReplica(blobApi, []);
    const s1 = session("lib-a");
    const s2 = session("lib-b");
    expect(
      initiatingCreateStillValid({
        mounted: true,
        initiating,
        currentReplica: initiating,
        initiatingSession: s1,
        currentSession: s1,
      }),
    ).toBe(true);
    expect(
      initiatingCreateStillValid({
        mounted: false,
        initiating,
        currentReplica: initiating,
        initiatingSession: s1,
        currentSession: s1,
      }),
    ).toBe(false);
    expect(
      initiatingCreateStillValid({
        mounted: true,
        initiating,
        currentReplica: readyReplica(blobApi, []),
        initiatingSession: s1,
        currentSession: s1,
      }),
    ).toBe(false);
    expect(
      initiatingCreateStillValid({
        mounted: true,
        initiating,
        currentReplica: { kind: "loading" },
        initiatingSession: s1,
        currentSession: s1,
      }),
    ).toBe(false);
    expect(
      initiatingCreateStillValid({
        mounted: true,
        initiating,
        currentReplica: initiating,
        initiatingSession: s1,
        currentSession: s2,
      }),
    ).toBe(false);
  });
});

describe("publishTemplateBackground Blank fast path", () => {
  it("returns Blank immediately without allocating/hashing Gray8", async () => {
    const gen = vi.spyOn(paperTemplates, "generateTemplateBackground");
    const put = vi.fn();
    const out = await publishTemplateBackground("blank", { put }, () => false);
    expect(out).toEqual({ background: { kind: "blank" }, sha256: null });
    expect(gen).not.toHaveBeenCalled();
    expect(put).not.toHaveBeenCalled();
    gen.mockRestore();
  });
});

describe("publishThenCreateNotebook session race", () => {
  const fakeBlob = {
    bytes: new Uint8Array([0xff, 0x80]),
    sha256: "a".repeat(64),
    background: {
      kind: "blob" as const,
      sha256: "a".repeat(64),
      byte_len: 2,
      format: { encoding: "gray8" as const, width: 1, height: 2, stride: 1 },
    },
  };

  it("deferred PUT + replica/session change does not queue CreateNotebook", async () => {
    const gen = vi.spyOn(paperTemplates, "generateTemplateBackground").mockResolvedValue(fakeBlob);
    let resolvePut!: (v: { sha256: string; byte_len: number; created: boolean }) => void;
    const putGate = new Promise<{ sha256: string; byte_len: number; created: boolean }>((r) => {
      resolvePut = r;
    });
    const put = vi.fn((_sha: string, bytes: Uint8Array) => putGate.then((ack) => ({ ...ack, byte_len: bytes.length })));
    const commits: unknown[] = [];
    const initiating = readyReplica({ put }, commits);
    const s1 = session("lib-a");
    const s2 = session("lib-b");
    let currentReplica: ReadyReplica | { kind: "loading" } = initiating;
    let currentSession = s1;
    let mounted = true;
    const commit = vi.fn(async () => {
      await commitStructureOn(initiating, {
        type: "create_notebook",
        notebook_id: "nb",
        title: "Late",
        first_page: {
          page_id: "pg",
          background: { kind: "blank" },
          render: initiating.createNotebookRender(),
        },
      });
      return true;
    });

    const pending = publishThenCreateNotebook({
      template: "ruled",
      blobApi: initiating.blobApi,
      stillValid: () =>
        initiatingCreateStillValid({
          mounted,
          initiating,
          currentReplica,
          initiatingSession: s1,
          currentSession,
        }),
      commit,
      isOnline: () => true,
    });

    await vi.waitFor(() => expect(put).toHaveBeenCalledTimes(1));
    expect(commit).not.toHaveBeenCalled();

    // Switch library / end viewer while upload is in flight.
    currentReplica = { kind: "loading" };
    currentSession = s2;
    mounted = false;

    resolvePut({ sha256: fakeBlob.sha256, byte_len: fakeBlob.bytes.length, created: true });
    const result = await pending;
    expect(result.ok).toBe(false);
    expect(result.notice).toMatch(/Nothing was changed/);
    expect(commit).not.toHaveBeenCalled();
    expect(commits).toHaveLength(0);
    gen.mockRestore();
  });

  it("successful normal creation still commits on the initiating replica", async () => {
    const gen = vi.spyOn(paperTemplates, "generateTemplateBackground").mockResolvedValue(fakeBlob);
    const put = vi.fn(async (sha: string, bytes: Uint8Array) => ({
      sha256: sha,
      byte_len: bytes.length,
      created: true,
    }));
    const commits: unknown[] = [];
    const initiating = readyReplica({ put }, commits);
    const s1 = session("lib-a");
    const backgrounds: unknown[] = [];

    const result = await publishThenCreateNotebook({
      template: "grid",
      blobApi: initiating.blobApi,
      stillValid: () =>
        initiatingCreateStillValid({
          mounted: true,
          initiating,
          currentReplica: initiating,
          initiatingSession: s1,
          currentSession: s1,
        }),
      commit: async (background) => {
        backgrounds.push(background);
        return commitStructureOn(initiating, {
          type: "create_notebook",
          notebook_id: initiating.mintId("notebook"),
          title: "Grid book",
          first_page: {
            page_id: initiating.mintId("page"),
            background,
            render: initiating.createNotebookRender(),
          },
        });
      },
      isOnline: () => true,
    });

    expect(result).toEqual({ ok: true, notice: null });
    expect(put).toHaveBeenCalledTimes(1);
    expect(backgrounds).toHaveLength(1);
    expect(backgrounds[0]).toMatchObject({ kind: "blob" });
    expect(commits).toHaveLength(1);
    expect(commits[0]).toMatchObject({ kind: "structure", structure: { type: "create_notebook", title: "Grid book" } });
    gen.mockRestore();
  });

  it("Blank create skips PUT and still commits when valid", async () => {
    const put = vi.fn();
    const commits: unknown[] = [];
    const initiating = readyReplica({ put }, commits);
    const s1 = session("lib-a");
    const result = await publishThenCreateNotebook({
      template: "blank",
      blobApi: initiating.blobApi,
      stillValid: () => true,
      commit: async (background) => {
        expect(background).toEqual({ kind: "blank" });
        return commitStructureOn(initiating, {
          type: "create_notebook",
          notebook_id: "nb",
          title: "Plain",
          first_page: { page_id: "pg", background, render: initiating.createNotebookRender() },
        });
      },
    });
    expect(result.ok).toBe(true);
    expect(put).not.toHaveBeenCalled();
    expect(commits).toHaveLength(1);
  });
});
