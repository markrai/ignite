// D1 product-mode screens: first-run claim and browser access requests. No token is ever shown
// or typed here: authorization arrives as an HttpOnly session cookie set by Ignite Server.

import { useEffect, useRef, useState, type FormEvent } from "react";
import { ControlError, defaultBrowserName, type ControlApi, type PairingCreated } from "../api/controlApi";
import { AppVersion } from "./AppVersion";

export function SetupScreen({ api, serverName, onClaimed }: { api: ControlApi; serverName: string; onClaimed: () => void }) {
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    api
      .claim(code.trim(), defaultBrowserName())
      .then(onClaimed)
      .catch((err: unknown) => {
        setError(err instanceof ControlError ? err.message : "Could not set up the server.");
        setBusy(false);
      });
  };
  return (
    <div className="center-screen">
      <form className="connect-card" onSubmit={submit} data-testid="setup-form" autoComplete="off">
        <h1>Ignite Server</h1>
        <p className="lead" data-testid="server-name">{serverName}</p>
        <h2 className="subhead">Set up this server</h2>
        <label className="field">
          <span className="field-label">Setup code</span>
          <input
            name="scribe-setup-code"
            autoComplete="off"
            spellCheck={false}
            autoCapitalize="characters"
            value={code}
            onChange={(e) => setCode(e.target.value)}
            data-testid="setup-code"
            placeholder="XXXX-XXXX"
          />
        </label>
        {error && (
          <div role="alert" className="error-text" data-testid="setup-error">
            {error}
          </div>
        )}
        <button className="btn primary" type="submit" disabled={busy || code.trim().length < 8} data-testid="claim">
          {busy ? "Setting up…" : "Claim Server"}
        </button>
        <p className="hint">The one-time setup code was printed when Ignite Server was installed. This browser becomes the server's administrator.</p>
      </form>
      <AppVersion />
    </div>
  );
}

type AccessState =
  | { kind: "idle"; notice: string | null }
  | { kind: "pending"; created: PairingCreated }
  | { kind: "error"; message: string };

export function RequestAccessScreen({
  api,
  serverName,
  revoked,
  onAuthorized,
}: {
  api: ControlApi;
  serverName: string;
  revoked: boolean;
  onAuthorized: () => void;
}) {
  const [name, setName] = useState(() => defaultBrowserName());
  const [state, setState] = useState<AccessState>({
    kind: "idle",
    notice: revoked ? "This browser's access was revoked or has ended. You can request access again." : null,
  });
  const live = useRef(true);
  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  // Poll the pending request until it is decided or expires (approval sets the cookie).
  useEffect(() => {
    if (state.kind !== "pending") return;
    const { request_id, request_secret } = state.created;
    const timer = setInterval(() => {
      api
        .poll(request_id, request_secret)
        .then((r) => {
          if (!live.current) return;
          if (r.state === "approved" || r.state === "delivered") {
            clearInterval(timer);
            onAuthorized();
          } else if (r.state === "denied") {
            clearInterval(timer);
            setState({ kind: "idle", notice: "The request was declined." });
          } else if (r.state === "expired") {
            clearInterval(timer);
            setState({ kind: "idle", notice: "The request expired. You can try again." });
          }
        })
        .catch(() => undefined); // transient: keep polling
    }, 1500);
    return () => clearInterval(timer);
  }, [state, api, onAuthorized]);

  const request = (e: FormEvent) => {
    e.preventDefault();
    api
      .requestAccess(name.trim() || defaultBrowserName())
      .then((created) => live.current && setState({ kind: "pending", created }))
      .catch((err: unknown) => live.current && setState({ kind: "error", message: err instanceof ControlError ? err.message : "Could not send the request." }));
  };

  return (
    <div className="center-screen">
      <form className="connect-card" onSubmit={request} data-testid="access-form" autoComplete="off">
        <h1>Ignite</h1>
        {state.kind === "pending" ? (
          <>
            <p className="lead">Waiting for approval on {serverName}</p>
            <p className="hint">In Ignite on an administrator's browser, open Settings → Server and allow the request showing this code:</p>
            <div className="comparison-code" data-testid="comparison-code">
              {state.created.comparison_code}
            </div>
            <button className="btn" type="button" onClick={() => setState({ kind: "idle", notice: null })}>
              Cancel
            </button>
          </>
        ) : (
          <>
            <p className="lead" data-testid="request-heading">
              Request access to {serverName}
            </p>
            {state.kind === "idle" && state.notice && (
              <div role="status" className="notice-text" data-testid="access-notice">
                {state.notice}
              </div>
            )}
            {state.kind === "error" && (
              <div role="alert" className="error-text" data-testid="access-error">
                {state.message}
              </div>
            )}
            <label className="field">
              <span className="field-label">This browser's name</span>
              <input value={name} maxLength={48} onChange={(e) => setName(e.target.value)} data-testid="browser-name" />
            </label>
            <button className="btn primary" type="submit" data-testid="request-access">
              Request Access
            </button>
            <p className="hint">An administrator approves new browsers. Trusted LAN only (plain HTTP).</p>
          </>
        )}
      </form>
      <AppVersion />
    </div>
  );
}
