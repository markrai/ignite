import { AppVersion } from "./AppVersion";
import { ThemeToggle } from "./ThemeToggle";
import type { LibrarySummary } from "../model/types";

interface Props {
  libraries: LibrarySummary[];
  onOpen: (id: string) => void;
  onDisconnect: () => void;
  /** D1 product mode: admin browsers reach Settings → Server (e.g. to approve a Kindle). */
  onSettings?: () => void;
  disconnectLabel?: string;
}

/** Libraries have no semantic title in the model: identify them by ID and counts only. */
export function shortId(id: string): string {
  return `${id.slice(0, 8)}…${id.slice(-4)}`;
}

export function LibraryChooser({ libraries, onOpen, onDisconnect, onSettings, disconnectLabel = "Disconnect" }: Props) {
  return (
    <div className="center-screen">
      <div className="chooser" data-testid="library-chooser">
        <h1>Ignite</h1>
        {libraries.length === 0 ? (
          <p data-testid="no-libraries">
            No synchronized libraries{onSettings ? ". Pair Ignite on Kindle from Settings → Server to sync its library here." : ""}
          </p>
        ) : (
          <ul>
            {libraries.map((l) => (
              <li key={l.library_id}>
                <button className="library-card" onClick={() => onOpen(l.library_id)} data-testid="library-card" title={l.library_id}>
                  <span className="library-kind">Library</span>
                  <span className="library-id">{shortId(l.library_id)}</span>
                  <span className="library-counts">
                    {l.notebook_count} {l.notebook_count === 1 ? "notebook" : "notebooks"} · {l.page_count}{" "}
                    {l.page_count === 1 ? "page" : "pages"}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
        {onSettings && (
          <button className="btn" onClick={onSettings} data-testid="settings">
            Settings
          </button>
        )}
        <div className="chooser-actions">
          <ThemeToggle />
          <button className="btn subtle" onClick={onDisconnect}>
            {disconnectLabel}
          </button>
        </div>
      </div>
      <AppVersion />
    </div>
  );
}
