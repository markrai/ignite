import { useState, type FormEvent } from "react";
import { AppVersion } from "./AppVersion";

interface Props {
  busy: boolean;
  error: string | null;
  onConnect: (token: string) => void;
}

/** Locked state. The token goes only into the Authorization header of same-origin `/v1` GETs. */
export function ConnectScreen({ busy, error, onConnect }: Props) {
  const [token, setToken] = useState("");
  const submit = (e: FormEvent) => {
    e.preventDefault();
    onConnect(token.trim());
    setToken("");
  };
  return (
    <div className="center-screen">
      <form className="connect-card" onSubmit={submit} data-testid="connect-form" autoComplete="off">
        <h1>Ignite</h1>
        <div className="field-row">
          <span className="field-label">Server</span>
          <span className="server-origin">{window.location.origin}</span>
        </div>
        <label className="field">
          <span className="field-label">Token</span>
          <input
            type="password"
            name="scribe-token"
            autoComplete="off"
            spellCheck={false}
            value={token}
            onChange={(e) => setToken(e.target.value)}
            data-testid="token-input"
            placeholder="Leave empty for a local no-auth server"
          />
        </label>
        {error && (
          <div role="alert" className="error-text" data-testid="connect-error">
            {error}
          </div>
        )}
        <button className="btn primary" type="submit" disabled={busy} data-testid="connect">
          {busy ? "Connecting…" : "Connect"}
        </button>
        <p className="hint">Read-only viewer. Trusted LAN only (plain HTTP).</p>
      </form>
      <AppVersion />
    </div>
  );
}
