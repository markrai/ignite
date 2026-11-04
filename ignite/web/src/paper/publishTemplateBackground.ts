// Publish a patterned template background before CreateNotebook (Slice 6B).
//
// Blank → no network. Patterned → generate Gray8 → PUT /v1/blobs/{sha} → return Blob ref.
// Does not queue CreateNotebook; callers must not submit the op until this resolves.

import type { BackgroundRef } from "../model/types";
import type { BlobApi } from "../api/blobApi";
import { TransientError } from "../api/replicaApi";
import * as paperTemplates from "./paperTemplates";
import type { PageTemplate } from "./paperTemplates";

export class TemplatePublishError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TemplatePublishError";
  }
}

export type OnlineProbe = () => boolean;

/** Default: browser `navigator.onLine` when available; otherwise assume online. */
export function browserOnline(): boolean {
  return typeof navigator === "undefined" || navigator.onLine !== false;
}

/**
 * Resolve the first-page background for a new notebook.
 * Blank returns immediately (no 4.6 MiB allocation/hash). Patterned templates require a
 * successful blob PUT; failure never invents a local-only ref.
 * Parity tests still call `generateTemplateBackground("blank")` directly.
 */
export async function publishTemplateBackground(
  template: PageTemplate,
  blobs: BlobApi,
  isOnline: OnlineProbe = browserOnline,
): Promise<{ background: BackgroundRef; sha256: string | null }> {
  if (template === "blank") {
    return { background: { kind: "blank" }, sha256: null };
  }
  const { bytes, sha256, background } = await paperTemplates.generateTemplateBackground(template);
  if (background.kind === "blank") {
    // Nonblank generator path should never yield Blank; refuse rather than skip the PUT.
    throw new TemplatePublishError("Couldn’t prepare the paper background. Nothing was created.");
  }
  if (!isOnline()) {
    throw new TemplatePublishError(
      "Patterned paper needs a network connection to publish its background. Nothing was created.",
    );
  }
  try {
    await blobs.put(sha256, bytes);
  } catch (e) {
    if (e instanceof TransientError) {
      throw new TemplatePublishError(
        "Couldn’t upload the paper background. Check the connection and try again. Nothing was created.",
      );
    }
    throw new TemplatePublishError(
      "Couldn’t publish the paper background. Nothing was created.",
    );
  }
  return { background, sha256 };
}
