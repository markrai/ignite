export interface TextEditorHeight {
  height: number;
  maxHeight: number;
  overflowY: "hidden" | "auto";
}

/** Fit derived editor content to the visible paper below its canonical anchor. */
export function fitTextEditorHeight(naturalHeight: number, lineHeight: number, remainingHeight: number): TextEditorHeight {
  const maxHeight = Math.max(1, remainingHeight);
  const desired = Math.max(lineHeight, naturalHeight);
  return {
    height: Math.min(desired, maxHeight),
    maxHeight,
    overflowY: desired > maxHeight ? "auto" : "hidden",
  };
}
