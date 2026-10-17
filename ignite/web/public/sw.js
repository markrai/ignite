// Lifecycle-only service worker for ignite-web update detection.
// APP_VERSION is rewritten at build time from the repo-root VERSION file.
// Byte changes here are what the browser uses to find a waiting worker.
//
// No fetch handler and no Cache Storage: IndexedDB remains the durable
// replica/outbox store. This worker does not make the app offline-capable.
const APP_VERSION = "__SCRIBE_VERSION__";

self.addEventListener("activate", (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener("message", (event) => {
  if (event.data && event.data.type === "SKIP_WAITING") {
    self.skipWaiting();
  }
});

void APP_VERSION;
