import { useSyncExternalStore } from "react";
import { currentTheme, subscribeTheme, toggleTheme } from "../app/theme";

/** Top-bar light/dark switch. Shows the mode a click switches to. */
export function ThemeToggle() {
  const theme = useSyncExternalStore(subscribeTheme, currentTheme);
  const next = theme === "dark" ? "light" : "dark";
  return (
    <button
      className="btn subtle icon theme-toggle"
      aria-label={`Switch to ${next} mode`}
      title={`Switch to ${next} mode`}
      data-testid="theme-toggle"
      data-theme={theme}
      onClick={toggleTheme}
    >
      <span aria-hidden>{theme === "dark" ? "☀" : "☾"}</span>
    </button>
  );
}
