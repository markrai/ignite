// Library sidebar: folders first (each expands to its notebooks), then the notebooks in no
// folder — the Kindle Home order. Folder edits (new, rename, delete, add/change/remove a
// notebook's folder) are durable replica operations: `commit` queues exactly one folder
// operation through the sync coordinator. What is shown is the reducer's projection of the
// snapshot plus pending folder operations, computed by the caller. The sort button only
// changes presentation (see model/librarySort.ts): folder membership and the canonical arrays
// are never touched, and nothing is committed.

import { useEffect, useMemo, useRef, useState } from "react";
import { orderLibrary, SORT_LABELS, type SortMode } from "../model/librarySort";
import type { SnapshotFolder, SnapshotNotebook } from "../model/types";
import { ProtocolViolation, validateTitle } from "../model/validate";
import type { FolderIntent } from "../replica/store";
import { FolderIcon, ListSortDescendingIcon, NotebookIcon } from "./Icons";

interface Props {
  notebooks: readonly SnapshotNotebook[];
  folders: readonly SnapshotFolder[];
  /** notebook_id → folder_id (projected). */
  membership: ReadonlyMap<string, string>;
  selectedId: string;
  onSelect: (n: SnapshotNotebook) => void;
  /** Null when folders can be edited; otherwise why not (shown as a tooltip). */
  readOnlyReason: string | null;
  mintFolderId: () => string;
  /** Queue one folder operation; resolves false when nothing was queued. */
  commit: (intent: FolderIntent) => Promise<boolean>;
  createNotebook: (title: string, folderId: string | null) => Promise<boolean>;
  renameNotebook: (notebookId: string, title: string) => Promise<boolean>;
  deleteNotebook: (notebookId: string) => Promise<boolean>;
  canDeleteNotebook: boolean;
  /** Replica-local display order and this browser's open history (never synced). */
  sortMode: SortMode;
  access: ReadonlyMap<string, number>;
  onCycleSort: () => void;
}

type Dialog =
  | { kind: "name"; heading: string; submit: string; initial: string; folderId: string | null }
  | { kind: "folder-menu"; folder: SnapshotFolder }
  | { kind: "notebook-menu"; notebook: SnapshotNotebook; folderId: string | null }
  | { kind: "pick"; notebook: SnapshotNotebook; exclude: string | null }
  | { kind: "notebook-name"; heading: string; submit: string; initial: string; notebookId: string | null; folderId: string | null }
  | { kind: "confirm-delete"; folder: SnapshotFolder }
  | { kind: "confirm-delete-notebook"; notebook: SnapshotNotebook };

function pages(n: SnapshotNotebook): string {
  return `${n.pages.length} ${n.pages.length === 1 ? "page" : "pages"}`;
}

export function NotebookSidebar({
  notebooks,
  folders,
  membership,
  selectedId,
  onSelect,
  readOnlyReason,
  mintFolderId,
  commit,
  createNotebook,
  renameNotebook,
  deleteNotebook,
  canDeleteNotebook,
  sortMode,
  access,
  onCycleSort,
}: Props) {
  const [dialog, setDialog] = useState<Dialog | null>(null);
  // Per-folder expand/collapse chosen by the user; otherwise a folder is expanded only while it
  // holds the selected notebook (so the selection is visible).
  const [expandedBy, setExpandedBy] = useState<ReadonlyMap<string, boolean>>(new Map());
  const editable = readOnlyReason === null;

  const members = useMemo(
    () => orderLibrary(notebooks, folders, membership, sortMode, access),
    [notebooks, folders, membership, sortMode, access],
  );

  const selectedFolder = membership.get(selectedId);
  const isOpen = (id: string) => expandedBy.get(id) ?? id === selectedFolder;
  const toggle = (id: string) => setExpandedBy((m) => new Map(m).set(id, !isOpen(id)));

  const run = async (intent: FolderIntent) => {
    setDialog(null);
    await commit(intent);
  };

  const notebookRow = (n: SnapshotNotebook, folderId: string | null) => {
    const selected = n.notebook_id === selectedId;
    return (
      <li key={n.notebook_id} className="notebook-row">
        <button
          className={`notebook ${selected ? "selected" : ""}`}
          aria-current={selected ? "true" : undefined}
          data-testid="notebook-item"
          data-notebook-id={n.notebook_id}
          data-folder-id={folderId ?? undefined}
          onClick={() => onSelect(n)}
        >
          <span className="notebook-title">
            <span className="row-icon" aria-hidden>
              <NotebookIcon />
            </span>{" "}
            {n.title}
          </span>
          <span className="notebook-pages">{pages(n)}</span>
        </button>
        <button
          className="btn subtle row-menu"
          aria-label={`${n.title} options`}
          data-testid="notebook-menu"
          disabled={!editable}
          title={readOnlyReason ?? undefined}
          onClick={() => setDialog({ kind: "notebook-menu", notebook: n, folderId })}
        >
          ⋯
        </button>
      </li>
    );
  };

  return (
    <>
      <div className="sidebar-head">
        <div className="sidebar-actions">
          <button
            className="btn icon subtle new-notebook"
            data-testid="new-notebook"
            disabled={!editable}
            aria-label="New notebook"
            title={readOnlyReason ?? "New notebook"}
            onClick={() =>
              setDialog({ kind: "notebook-name", heading: "New notebook", submit: "Create", initial: `Notebook ${notebooks.length + 1}`, notebookId: null, folderId: null })
            }
          >
            <NotebookIcon />
          </button>
          <button
            className="btn icon subtle new-folder"
            data-testid="new-folder"
            disabled={!editable}
            aria-label="New folder"
            title={readOnlyReason ?? "New folder"}
            onClick={() => setDialog({ kind: "name", heading: "New folder", submit: "Create", initial: `Folder ${folders.length + 1}`, folderId: null })}
          >
            <FolderIcon />
          </button>
          <button
            className="btn icon subtle sort-order"
            data-testid="sort-order"
            data-sort-mode={sortMode}
            aria-label={`Change sort order — ${SORT_LABELS[sortMode]}`}
            title={`Change sort order — ${SORT_LABELS[sortMode]}`}
            onClick={onCycleSort}
          >
            <ListSortDescendingIcon />
          </button>
        </div>
        {readOnlyReason && (
          <p className="sidebar-readonly" data-testid="library-readonly" role="status">
            {readOnlyReason}
          </p>
        )}
      </div>
      <ul>
        {members.folders.map((f) => {
          const list = members.byFolder.get(f.folder_id) ?? [];
          const expanded = isOpen(f.folder_id);
          return (
            <li key={f.folder_id} className="folder" data-testid="folder-item" data-folder-id={f.folder_id}>
              <div className="folder-row">
                <button
                  className="folder-toggle"
                  aria-expanded={expanded}
                  data-testid="folder-toggle"
                  onClick={() => toggle(f.folder_id)}
                >
                  <span className="folder-title">
                    <span className="row-icon" aria-hidden>
                      <FolderIcon />
                    </span>{" "}
                    {f.title}
                  </span>
                  <span className="notebook-pages">{list.length}</span>
                </button>
                <button
                  className="btn subtle row-menu"
                  aria-label={`${f.title} folder options`}
                  data-testid="folder-menu"
                  disabled={!editable}
                  title={readOnlyReason ?? undefined}
                  onClick={() => setDialog({ kind: "folder-menu", folder: f })}
                >
                  ⋯
                </button>
              </div>
              {expanded && (
                <ul className="folder-notebooks">
                  {list.length === 0 ? (
                    <li className="folder-empty">
                      {editable ? (
                        <button
                          className="btn subtle"
                          data-testid="folder-new-notebook"
                          onClick={() =>
                            setDialog({
                              kind: "notebook-name",
                              heading: "New notebook",
                              submit: "Create",
                              initial: `Notebook ${notebooks.length + 1}`,
                              notebookId: null,
                              folderId: f.folder_id,
                            })
                          }
                        >
                          + Notebook
                        </button>
                      ) : (
                        "Empty"
                      )}
                    </li>
                  ) : (
                    list.map((n) => notebookRow(n, f.folder_id))
                  )}
                </ul>
              )}
            </li>
          );
        })}
        {members.top.map((n) => notebookRow(n, null))}
      </ul>
      {dialog && (
        <SidebarDialog
          dialog={dialog}
          folders={folders}
          onClose={() => setDialog(null)}
          onOpen={setDialog}
          onSubmitName={async (title, folderId) => {
            if (folderId !== null) return run({ type: "rename_folder", folder_id: folderId, title });
            let id: string;
            try {
              id = mintFolderId();
            } catch {
              setDialog(null);
              return;
            }
            return run({ type: "create_folder", folder_id: id, title });
          }}
          onSubmitNotebook={async (title, notebookId, folderId) => {
            setDialog(null);
            if (notebookId) {
              await renameNotebook(notebookId, title);
              return;
            }
            await createNotebook(title, folderId);
          }}
          onDeleteNotebook={async (id) => {
            setDialog(null);
            await deleteNotebook(id);
          }}
          canDeleteNotebook={canDeleteNotebook}
          run={run}
        />
      )}
    </>
  );
}

function SidebarDialog({
  dialog,
  folders,
  onClose,
  onOpen,
  onSubmitName,
  onSubmitNotebook,
  onDeleteNotebook,
  canDeleteNotebook,
  run,
}: {
  dialog: Dialog;
  folders: readonly SnapshotFolder[];
  onClose: () => void;
  onOpen: (d: Dialog) => void;
  onSubmitName: (title: string, folderId: string | null) => Promise<void>;
  onSubmitNotebook: (title: string, notebookId: string | null, folderId: string | null) => Promise<void>;
  onDeleteNotebook: (notebookId: string) => Promise<void>;
  canDeleteNotebook: boolean;
  run: (intent: FolderIntent) => Promise<void>;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  let heading: string;
  let body: React.ReactNode;
  switch (dialog.kind) {
    case "name":
      heading = dialog.heading;
      body = <NameForm initial={dialog.initial} submit={dialog.submit} onCancel={onClose} onSubmit={(t) => void onSubmitName(t, dialog.folderId)} />;
      break;
    case "folder-menu":
      heading = dialog.folder.title;
      body = (
        <div className="dialog-actions vertical">
          <button
            className="btn"
            data-testid="action-new-notebook"
            onClick={() =>
              onOpen({
                kind: "notebook-name",
                heading: "New notebook",
                submit: "Create",
                initial: "Notebook",
                notebookId: null,
                folderId: dialog.folder.folder_id,
              })
            }
          >
            New notebook
          </button>
          <button
            className="btn"
            data-testid="action-rename"
            onClick={() => onOpen({ kind: "name", heading: "Rename folder", submit: "Save", initial: dialog.folder.title, folderId: dialog.folder.folder_id })}
          >
            Rename
          </button>
          <button className="btn" data-testid="action-delete" onClick={() => onOpen({ kind: "confirm-delete", folder: dialog.folder })}>
            Delete
          </button>
          <button className="btn subtle" onClick={onClose}>
            Cancel
          </button>
        </div>
      );
      break;
    case "notebook-menu": {
      const { notebook, folderId } = dialog;
      body = (
        <div className="dialog-actions vertical">
          <button
            className="btn"
            data-testid="action-rename-notebook"
            onClick={() =>
              onOpen({ kind: "notebook-name", heading: "Rename notebook", submit: "Save", initial: notebook.title, notebookId: notebook.notebook_id, folderId: null })
            }
          >
            Rename
          </button>
          <button
            className="btn"
            data-testid="action-delete-notebook"
            disabled={!canDeleteNotebook}
            title={canDeleteNotebook ? undefined : "A library needs at least one notebook."}
            onClick={() => onOpen({ kind: "confirm-delete-notebook", notebook })}
          >
            Delete
          </button>
          {folderId === null ? (
            <button className="btn" data-testid="action-add-to-folder" onClick={() => onOpen({ kind: "pick", notebook, exclude: null })}>
              Add to folder
            </button>
          ) : (
            <>
              <button className="btn" data-testid="action-change-folder" onClick={() => onOpen({ kind: "pick", notebook, exclude: folderId })}>
                Change folder
              </button>
              <button
                className="btn"
                data-testid="action-remove-from-folder"
                onClick={() => void run({ type: "set_notebook_folder", notebook_id: notebook.notebook_id })}
              >
                Remove from folder
              </button>
            </>
          )}
          <button className="btn subtle" onClick={onClose}>
            Cancel
          </button>
        </div>
      );
      heading = notebook.title;
      break;
    }
    case "pick": {
      const choices = folders.filter((f) => f.folder_id !== dialog.exclude);
      heading = `Move “${dialog.notebook.title}” to`;
      body = (
        <div className="dialog-actions vertical">
          {choices.length === 0 ? (
            <p className="dialog-text">{dialog.exclude === null ? "No folders yet. Create one with + Folder." : "No other folder. Create one with + Folder."}</p>
          ) : (
            <div className="picker">
              {choices.map((f) => (
                <button
                  key={f.folder_id}
                  className="btn"
                  data-testid="pick-folder"
                  data-folder-id={f.folder_id}
                  onClick={() => void run({ type: "set_notebook_folder", notebook_id: dialog.notebook.notebook_id, folder_id: f.folder_id })}
                >
                  {f.title}
                </button>
              ))}
            </div>
          )}
          <button className="btn subtle" onClick={onClose}>
            Cancel
          </button>
        </div>
      );
      break;
    }
    case "confirm-delete":
      heading = `Delete folder “${dialog.folder.title}”?`;
      body = (
        <>
          <p className="dialog-text">Its notebooks move back to the main list. No notebook is deleted.</p>
          <div className="dialog-actions">
            <button className="btn subtle" onClick={onClose}>
              Cancel
            </button>
            <button className="btn primary" data-testid="confirm-delete" onClick={() => void run({ type: "delete_folder", folder_id: dialog.folder.folder_id })}>
              Delete
            </button>
          </div>
        </>
      );
      break;
    case "notebook-name":
      heading = dialog.heading;
      body = (
        <NameForm
          initial={dialog.initial}
          submit={dialog.submit}
          inputTestId="notebook-name-input"
          onCancel={onClose}
          onSubmit={(t) => void onSubmitNotebook(t, dialog.notebookId, dialog.folderId)}
        />
      );
      break;
    case "confirm-delete-notebook":
      heading = `Delete notebook “${dialog.notebook.title}”?`;
      body = (
        <>
          <p className="dialog-text">This notebook and its pages are removed from the library. This cannot be undone here.</p>
          <div className="dialog-actions">
            <button className="btn subtle" onClick={onClose}>
              Cancel
            </button>
            <button className="btn primary" data-testid="confirm-delete-notebook" onClick={() => void onDeleteNotebook(dialog.notebook.notebook_id)}>
              Delete
            </button>
          </div>
        </>
      );
      break;
  }
  const dialogTestId =
    dialog.kind === "notebook-name" || dialog.kind === "confirm-delete-notebook" || dialog.kind === "notebook-menu" ? "notebook-dialog" : "folder-dialog";
  return (
    <div className="dialog-scrim" onClick={onClose}>
      <div className="dialog" role="dialog" aria-modal="true" aria-label={heading} data-testid={dialogTestId} onClick={(e) => e.stopPropagation()}>
        <h2 className="dialog-heading">{heading}</h2>
        {body}
      </div>
    </div>
  );
}

function NameForm({
  initial,
  submit,
  onCancel,
  onSubmit,
  inputTestId = "folder-name-input",
}: {
  initial: string;
  submit: string;
  onCancel: () => void;
  onSubmit: (title: string) => void;
  inputTestId?: string;
}) {
  const [text, setText] = useState(initial);
  const [error, setError] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    input.current?.select();
  }, []);
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        const title = text.trim();
        try {
          validateTitle(title, "title");
        } catch (err) {
          setError(err instanceof ProtocolViolation ? "Enter a name of 1–96 characters." : String(err));
          return;
        }
        onSubmit(title);
      }}
    >
      <input
        ref={input}
        className="dialog-input"
        data-testid={inputTestId}
        aria-label="Name"
        value={text}
        maxLength={96}
        autoFocus
        onChange={(e) => {
          setText(e.target.value);
          setError(null);
        }}
      />
      {error && (
        <p className="dialog-text error-text" role="alert">
          {error}
        </p>
      )}
      <div className="dialog-actions">
        <button type="button" className="btn subtle" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className="btn primary" data-testid="dialog-submit">
          {submit}
        </button>
      </div>
    </form>
  );
}
