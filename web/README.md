# execlaw web SPA

The chat-first SPA for execlaw. React + Vite + react-bootstrap + Bootstrap Icons.

Phase 6a scope (this scaffold): boot detection (`/api/ping`), setup wizard, login, JWT-backed auth context, /chat placeholder. The full chat shell (sidebar, thread list, streaming tokens, inline approval card) lands in the next 6a pass.

## Quick start (hands-on test)

```sh
# Terminal 1 — Rust server. The `serve` subcommand binds 127.0.0.1:3030
# by default; --no-encrypt skips SQLCipher so the dev DB is portable.
cd /path/to/execlaw
cargo run -p execlaw -- serve --no-encrypt

# Terminal 2 — Vite dev (proxies /api → :3030, no CORS needed).
cd web
npm install        # first time
npm run dev
# → http://localhost:5173
```

The first page load probes `GET /api/ping`:

- `setup` → routes you to `/setup`. Fill in display name + admin password (≥ 8 chars) + optional email. The form posts `/api/setup`, persists the access + refresh JWTs to localStorage, fetches `/api/admin/me`, and lands you on the `/chat` placeholder.
- `pong` → routes to `/login`. Enter the admin password; same JWT + /me flow.
- network failure → an unobtrusive "Can't reach the server" panel with a retry button.

After signup/login you can hard-reload the page; the auth context restores tokens from localStorage and re-validates them via `/api/admin/me` (with one refresh-token retry on 401).

## Scripts

- `npm run dev` — Vite dev server with HMR + `/api → :3030` proxy.
- `npm run build` — typecheck + production bundle into `dist/`.
- `npm run preview` — serve `dist/` on `:4173` for prod-mode smoke testing.
- `npm test` — vitest run, jsdom env.
- `npm run test:watch` — vitest watch mode.
- `npm run lint` — `tsc --noEmit`.
- `npm run size` — enforce bundle-size budgets after `npm run build`.

## Layout

```
src/
  main.tsx                 Vite entry; mounts <App/> in <StrictMode>
  App.tsx                  Router shell + AuthProvider
  api/
    client.ts              Fetch wrapper, ApiError, status mapping
    endpoints.ts           Typed wrappers: ping, setup, login, me, refresh, logout
  auth/
    AuthContext.tsx        Tokens + user + signIn/signOut + boot probe
    tokens.ts              localStorage persistence
  routes/
    AppBoot.tsx            First-render setup-state probe → setup/login/chat
    SetupWizard.tsx        First-run controller-account form
    Login.tsx              Single-controller password form
    Chat.tsx               Placeholder until full chat shell lands
  styles/theme.scss        Dark-default Bootstrap theme + app shell styles
  __tests__/               vitest specs (28 tests today)
scripts/bundle-size.mjs    Bundle-budget enforcer (axiom #14 for the SPA)
```

## Backend awareness of setup state

- `GET /api/ping` returns plain text `setup` (no controller user yet) or `pong` (initialized). Used as the SPA's first probe.
- `POST /api/setup` writes the operator into the `users` table (single-controller mode), returns the JWT pair.
- `POST /api/login` reads via `UserStore::get_first()` and verifies against the stored Argon2id hash.
- `GET /api/admin/me` returns the logged-in user's profile so the SPA can render the `⚙ user@email` affordance and re-validate tokens.

The Rust server tests cover ping-before-vs-after-setup, /me-without-auth, /me-with-token, login-before-setup-conflict, and setup-rejects-empty-display-name.

## Deps

Tight on purpose. No heavyweight state libraries until a real consumer arrives:

- `react`, `react-dom` (18.3) — React.
- `react-router-dom` (6) — routing.
- `react-bootstrap` (2.10) + `bootstrap` (5.3 SCSS) + `bootstrap-icons` (1.11) — UI + icon set.
- `gsap` (3.12) + `@gsap/react` (2.1) — page-transition animations.
- `vitest` + `@testing-library/react` + `jsdom` — tests.
- `vite` + `@vitejs/plugin-react` + `typescript` + `sass` — build.

Native iOS / Android targets (Phase 6e+) will land via a parallel
component layer — Tamagui or similar — at port time. For now the SPA
is plain React on the DOM.
