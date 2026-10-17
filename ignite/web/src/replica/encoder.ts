// The store's synchronous envelope encoder: every browser operation is built by the Rust bridge.

import type { ProtocolBridge } from "../wasm/bridge";
import { isErase, isFolder, isStructure, isTextAdd, isTextDelete, isTextUpdate, isUndo, type Encoder } from "./store";

export function bridgeEncoder(bridge: ProtocolBridge, libraryId: string): Encoder {
  return (replicaId, clientSeq, d) => {
    if (isFolder(d)) {
      return bridge.encodeFolderOp({
        library_id: libraryId,
        replica_id: replicaId,
        client_seq: clientSeq,
        operation_id: d.operation_id,
        operation: d.folder,
      });
    }
    if (isStructure(d)) {
      return bridge.encodeStructureOp({
        library_id: libraryId,
        replica_id: replicaId,
        client_seq: clientSeq,
        operation_id: d.operation_id,
        operation: d.structure,
      });
    }
    if (isUndo(d)) {
      return bridge.encodeDeleteStrokeLineage({
        library_id: libraryId,
        replica_id: replicaId,
        client_seq: clientSeq,
        operation_id: d.operation_id,
        notebook_id: d.notebook_id,
        page_id: d.page_id,
        root_id: d.root_id,
      });
    }
    if (isErase(d)) {
      return bridge.encodeEraseRegion({
        library_id: libraryId,
        replica_id: replicaId,
        client_seq: clientSeq,
        operation_id: d.operation_id,
        notebook_id: d.notebook_id,
        page_id: d.page_id,
        observed_roots: d.erase.observed_roots,
        path: d.erase.path,
      });
    }
    if (isTextAdd(d) || isTextUpdate(d)) {
      const fn = isTextAdd(d) ? "encode_add_text" : "encode_update_text";
      return bridge.encodeTextOp(fn, {
        library_id: libraryId,
        replica_id: replicaId,
        client_seq: clientSeq,
        operation_id: d.operation_id,
        notebook_id: d.notebook_id,
        page_id: d.page_id,
        text: d.text,
      });
    }
    if (isTextDelete(d)) {
      return bridge.encodeDeleteText({
        library_id: libraryId,
        replica_id: replicaId,
        client_seq: clientSeq,
        operation_id: d.operation_id,
        notebook_id: d.notebook_id,
        page_id: d.page_id,
        text_id: d.text_id,
      });
    }
    return bridge.encodeAddStroke({
      library_id: libraryId,
      replica_id: replicaId,
      client_seq: clientSeq,
      operation_id: d.operation_id,
      notebook_id: d.notebook_id,
      page_id: d.page_id,
      stroke_id: d.stroke.stroke_id,
      style: d.stroke.style,
      points: d.stroke.points,
    });
  };
}
