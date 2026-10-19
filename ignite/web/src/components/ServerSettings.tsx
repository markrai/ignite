// D1 minimal Ignite Server administration: identity/status, pending access requests
// (Allow/Deny with the comparison code), connected devices (Revoke), and (read-only) backup
// restore jobs. Admin sessions only.

import { useCallback, useEffect, useState } from "react";
import { ControlError, type ControlApi, type DeviceRecord, type PendingRequest, type ServerStatusAdmin } from "../api/controlApi";
import { AppVersion } from "./AppVersion";
import { RestoreJobs } from "./RestoreJobs";

function when(ms: number | null): string {
  if (!ms) return "never";
  const d = Date.now() - ms;
  if (d < 90_000) return "just now";
  if (d < 3_600_000) return `${Math.round(d / 60_000)} min ago`;
  if (d < 86_400_000) return `${Math.round(d / 3_600_000)} h ago`;
  return new Date(ms).toLocaleDateString();
}

export function ServerSettings({ api, onClose }: { api: ControlApi; onClose: () => void }) {
  const [server, setServer] = useState<ServerStatusAdmin | null>(null);
  const [pending, setPending] = useState<PendingRequest[]>([]);
  const [devices, setDevices] = useState<DeviceRecord[]>([]);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [s, p, d] = await Promise.all([api.server(), api.pending(), api.devices()]);
      setServer(s);
      setPending(p);
      setDevices(d);
      setError(null);
    } catch (e) {
      setError(e instanceof ControlError ? e.message : "Could not load server settings.");
    }
  }, [api]);

  useEffect(() => {
    const first = setTimeout(() => void load(), 0);
    const t = setInterval(() => void load(), 2500); // pending requests appear without reload
    return () => {
      clearTimeout(first);
      clearInterval(t);
    };
  }, [load]);

  const act = (p: Promise<void>) =>
    p.then(load).catch((e: unknown) => setError(e instanceof ControlError ? e.message : "That did not work."));

  return (
    <div className="settings" data-testid="server-settings">
      <header className="topbar">
        <button className="btn subtle" onClick={onClose} data-testid="settings-close">
          ‹ Notebooks
        </button>
        <span className="brand">Settings · Server</span>
      </header>
      <div className="settings-body">
        {error && (
          <div role="alert" className="error-text">
            {error}
          </div>
        )}
        <section className="settings-card" data-testid="server-card">
          <h2>{server?.server_name ?? "Ignite Server"}</h2>
          {server && (
            <dl className="kv">
              <dt>Server ID</dt>
              <dd className="mono" data-testid="server-id">{server.server_id}</dd>
              <dt>Version</dt>
              <dd>{server.version}</dd>
              <dt>Running since</dt>
              <dd>{new Date(server.started_ms).toLocaleString()}</dd>
              <dt>Libraries</dt>
              <dd>{server.library_count}</dd>
              <dt>Authorized devices</dt>
              <dd>{server.device_count}</dd>
            </dl>
          )}
        </section>

        <section className="settings-card">
          <h2>Pending requests</h2>
          {pending.length === 0 && <p className="hint">No one is waiting for access.</p>}
          <ul className="rows">
            {pending.map((r) => (
              <li key={r.request_id} className="row" data-testid="pending-request">
                <div>
                  <div className="row-title">{r.display_name}</div>
                  <div className="hint">{r.client_type === "kindle" ? "Kindle" : "Browser"} · expires in {Math.max(1, Math.round(r.expires_in_s / 60))} min</div>
                </div>
                <div className="comparison-code small" data-testid="pending-code">
                  {r.comparison_code}
                </div>
                <div className="row-actions">
                  <button className="btn primary" onClick={() => void act(api.decide(r.request_id, true))} data-testid="allow">
                    Allow
                  </button>
                  <button className="btn" onClick={() => void act(api.decide(r.request_id, false))} data-testid="deny">
                    Deny
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </section>

        <section className="settings-card">
          <h2>Connected devices</h2>
          <ul className="rows">
            {devices.map((d) => (
              <li key={d.device_id} className={`row ${d.revoked ? "revoked" : ""}`} data-testid="device-row" data-device-id={d.device_id} data-revoked={d.revoked}>
                <div>
                  <div className="row-title">
                    {d.display_name}
                    {d.current && <span className="tag">this browser</span>}
                    {d.role === "admin" && <span className="tag">admin</span>}
                  </div>
                  <div className="hint">
                    {d.client_type === "kindle" ? "Kindle" : "Browser"} · {d.revoked ? "revoked" : `last seen ${when(d.last_seen_ms)}`}
                  </div>
                </div>
                {!d.revoked && !d.current && (
                  <div className="row-actions">
                    <button className="btn" onClick={() => void act(api.revoke(d.device_id))} data-testid="revoke">
                      Revoke
                    </button>
                  </div>
                )}
              </li>
            ))}
          </ul>
        </section>
        <RestoreJobs api={api} />
        <p className="hint">Trusted LAN only: plain HTTP, authenticated devices. Do not expose Ignite Server to the Internet.</p>
      </div>
      <AppVersion />
    </div>
  );
}
