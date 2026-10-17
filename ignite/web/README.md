# ignite-web

Browser notebook replica for **Ignite**: view and edit notebooks synchronized to a
self-hosted Ignite Server, with durable IndexedDB outbox and offline retries.

```text
Ignite Server ──▶ GET /v1/... snapshot + ops
        └──────────────────────────┤  ignite-web ──▶ Canvas 2D
          POST /v1/.../operations  ◀── IndexedDB outbox ◀── authoring
          /control/v1/*            ◀── pairing / sessions (product mode)
```

## Layout

| Path | Role |
|------|------|
| `src/` | React app, replica, WASM bridge loader |
| `src/wasm/scribe_web_wasm.wasm` | Built protocol bridge (internal artifact name unchanged) |
| `scripts/` | Production build orchestrator (Cargo WASM + Vite) |
| `public/` | Manifest, icons, lifecycle-only `sw.js` |
| `tests/` | Vitest unit/parity tests |
| `e2e/` | Playwright against a disposable local server |

**Compatibility (do not rename casually):** IndexedDB `scribe-web-sync` and
`scribe-web-prefs`, BroadcastChannel `scribe-web-sync:*`, WASM crate/artifact
`scribe-web-wasm` / `scribe_web_wasm.wasm`, sync discovery `_scribe._tcp`.

**PWA identity:** manifest `id` is the stable `/ignite-web` (version-independent).
Changing from the prior `/scribe-web/<version>` id means existing installed PWAs
may be treated as a separate application. IndexedDB and other origin-scoped
storage are not deleted or reset by this change.

## Version

Authoritative product version: [`../VERSION`](../VERSION) (currently **0.9.11**).
Vite injects it into the bundle and service worker. The PWA `id` does not include
the version.

## Production build

`npm run build` is the authoritative production path. It:

1. Builds `scribe-web-wasm` with Cargo (`--release --locked --target wasm32-unknown-unknown`)
2. Copies the fresh artifact to `src/wasm/scribe_web_wasm.wasm`
3. Runs TypeScript checking and the Vite production bundle
4. SHA-256-verifies that the fingerprinted WASM under `dist/` matches the Cargo output

A failed Cargo step fails the production build. There is no fallback to a stale
checked-in `.wasm`. Do not ship by manually copying an older module into `src/wasm/`.

Prerequisites: Node 20+, Rust with the `wasm32-unknown-unknown` target, and a
workspace `Cargo.lock` that matches the tree. Optional: set `CARGO_TARGET_DIR`
to redirect Cargo's target directory (honored by the orchestrator).

```bash
cd ignite/web
npm ci
npm run build          # Cargo WASM + tsc + vite + hash verify
npm run build:wasm     # WASM only (same Cargo path as production)
```

Standalone WASM from the repo root (requires Node; thin wrapper over the same script):

```bash
bash tools/build_web_wasm.sh
# or: node ignite/web/scripts/build-wasm.mjs
```

## Develop

```bash
cd ignite/web
npm ci
npm run typecheck
npm run lint
npm test
npm run build

# Dev UI (proxies /v1 and /control to a local server)
# Terminal A: run Ignite Server or sync server on :8787
# Terminal B:
npm run dev
```

Playwright (builds `dist/`, starts disposable servers — never Vega). Chromium is
the verified E2E browser in this migration; Firefox/WebKit remain unverified:

```bash
npx playwright install chromium   # once
npm run test:e2e
```

## Rust bridge tests

```bash
# From repository root
cargo test -p scribe-web-wasm --locked
cargo test -p scribe-sync-tool --test web_fixtures --locked
# Regenerate pinned fixtures only when intentional:
# SCRIBE_REGEN_WEB_FIXTURES=1 cargo test -p scribe-sync-tool --test web_fixtures --locked
```

## Product mode

On load ignite-web calls `/control/v1/status`. An Ignite Server shows setup /
request-access flows; developer-token mode remains available for local sync-only
servers. Sessions use HttpOnly cookies; developer bearers stay in `sessionStorage`.

## License

AGPL-3.0-only with the Ignite workspace. Font and third-party notices live under
`ignite/assets/` and `ignite/licenses/`.
