import { APP_VERSION_LABEL } from "../version";

export function AppVersion() {
  return (
    <span className="app-version" data-testid="app-version">
      {APP_VERSION_LABEL}
    </span>
  );
}
