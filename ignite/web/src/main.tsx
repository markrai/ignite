import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app/App";
import { initTheme } from "./app/theme";
import { UpdateBanner } from "./components/UpdateBanner";
import { startInstalledPwaFullscreen } from "./pwa/fullscreen";
import "./styles.css";

initTheme();
startInstalledPwaFullscreen();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
    <UpdateBanner />
  </StrictMode>,
);
