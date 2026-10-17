import { useEffect, useRef, useState } from "react";
import { startUpdateWatcher, type UpdateHandle } from "../pwa/registerUpdate";

/** Bottom-center toast when a newer ignite-web bundle is waiting to activate. */
export function UpdateBanner() {
  const [visible, setVisible] = useState(false);
  const handle = useRef<UpdateHandle | null>(null);

  useEffect(() => {
    const ctl = startUpdateWatcher(setVisible);
    handle.current = ctl;
    return () => ctl.stop();
  }, []);

  if (!visible) return null;

  return (
    <div className="update-toast" role="status" data-testid="pwa-update">
      <span>New version available!</span>
      <span className="update-toast-actions">
        <button type="button" className="btn primary" onClick={() => handle.current?.apply()}>
          Update now
        </button>
        <button type="button" className="btn subtle" onClick={() => handle.current?.dismiss()}>
          Later
        </button>
      </span>
    </div>
  );
}
