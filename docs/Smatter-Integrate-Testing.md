# Testing the videocall ↔ smatter chat integration (local)

How to run videocall's in-meeting chat against the **smatter** (`jmap-chat-server`)
backend on a local, **same-origin edge**. For the *why* (edge proxy, first-party
SSE cookie, browser-PKCE rationale), see the canonical docs in the
`labs-workspace` repo: `docs/sse-auth-edge-proxy.md` and
`docs/sse-edge-auth-local-setup.md`. This guide is the videocall-specific quick start.

## Architecture (local)

All browser traffic goes through a single **host Caddy** edge on `:443`, serving
three `*.localhost` origins so the auth cookie / id_token stay first-party:

| Origin | Serves | Upstream |
|---|---|---|
| `https://id.localhost` | identity-service (OIDC issuer) | `:8081` |
| `https://chat.localhost` | cc7 UI + smatter (JMAP/SSE) | cc7 `dx serve` `:8080`, smatter `:8443` |
| `https://meet.localhost` | videocall UI + meeting-api + meeting WebSocket | UI `:3002`, meeting-api `:8082`, **websocket-api host `:8086`** |

**Auth:** browser **PKCE** (public client). The browser exchanges the auth code at
`id.localhost` and stores the **id_token** in `sessionStorage` (`vc_id_token`); that
**id_token is the Bearer** sent to smatter for JMAP + the SSE session. It is **not**
the access_token, and there is **no** server-side token exchange.

**Meeting signaling:** the WebSocket rides the edge as `wss://meet.localhost/lobby`
→ videocall `websocket-api` (host `:8086`, container `:8080`). Host `:8080` is
reserved for **cc7's `dx serve`** — the two collide otherwise. A page served over
`https://` cannot open an insecure `ws://localhost` (Safari blocks it as mixed
content), so the meeting WS must be `wss://` via the edge.

## Prerequisites

- `/etc/hosts`: `127.0.0.1 id.localhost chat.localhost meet.localhost`
- Caddy installed (host edge); Docker (videocall + identity-service stacks);
  smatter, cc7, and Scylla checked out and runnable.

## 1. Register OIDC clients (in identity-service at `id.localhost`)

Register **two public clients** with **Require PKCE (S256)** — public means
**token-endpoint auth = none, no client secret**:

- **videocall** — redirect `https://meet.localhost/auth/callback`, post-logout `https://meet.localhost`
- **cc7** — redirect `https://chat.localhost/auth/callback`, post-logout `https://chat.localhost/logout`

> Never commit a real client secret. Public PKCE clients don't use one; if you ever
> use a confidential client, the secret goes in a gitignored `.env`, never in a
> tracked file. (A gitleaks gate enforces this.)

## 2. Configure each service (use **your** client ids — placeholders below)

- **videocall** `.env` (see `.env.example`): `OAUTH_FLOW=pkce`, `OAUTH_BROWSER_PKCE=true`,
  `OAUTH_CLIENT_ID=<your-videocall-client-id>`,
  `OAUTH_REDIRECT_URL=https://meet.localhost/auth/callback`,
  `API_BASE_URL=https://meet.localhost`, `ACTIX_UI_BACKEND_URL=wss://meet.localhost`.
- **smatter** `server/settings.toml`: add **both** client ids to `custom_audiences`
  (`["<your-videocall-client-id>", "<your-cc7-client-id>"]`). The OIDC client secret
  is supplied via env, not the committed file.
- **cc7**: set `IDENTITY_CLIENT_ID=<your-cc7-client-id>` (e.g. via `run-debug.sh`) so
  the browser PKCE client id matches what smatter accepts.

## 3. Start the stack (order matters)

1. **identity-service** — `docker compose up -d` (`:8081`).
2. **host Caddy** — `caddy run --config ./Caddyfile` in `jmap-chat-server` (owns `:443`).
3. **Scylla** — start the `scylla-node` container.
4. **smatter** — `./fnxlabs.sh` (after Scylla + Caddy; it does OIDC discovery against
   `https://id.localhost`). SSE must be enabled to receive chat/meeting events.
5. **cc7** — `./run-debug.sh` (`dx serve` on `:8080`).
6. **videocall** —
   `docker compose --env-file .env -f docker/docker-compose.yaml up postgres nats meeting-api websocket-api dioxus-ui`.

## 4. Verify

```bash
curl -sk https://id.localhost/.well-known/openid-configuration | grep -o '"issuer":"[^"]*"'
# → "issuer":"https://id.localhost"
curl -sk -o /dev/null -w '%{http_code}\n' https://meet.localhost/lobby
# → 400  (reaches websocket-api — NOT 200, which would mean it fell through to the UI)
```

## 5. Test the loop

1. Two users in **separate browsers** (separate cookie jars). Browse
   `https://meet.localhost` → you bounce to `id.localhost` to log in via PKCE → back
   to videocall.
2. Both join the **same meeting** → confirm they see each other (the `wss://…/lobby`
   connection succeeds in the console — no mixed-content block).
3. Chat in the meeting → it fans out to both participants over SSE, and appears
   **live** in cc7's `chat.localhost` group for the same topic — and a message posted
   in cc7 appears in the videocall chat. (A brand-new meeting group should surface in
   cc7's list without a manual refresh.)

## Troubleshooting

- **Chat 401 "Missing or invalid Authorization header"** → the browser has no
  id_token: you're not on the PKCE flow. Confirm `OAUTH_FLOW=pkce` and that the
  client is registered **public**; re-login (clear site data) so the exchange runs.
- **Participants isolated / "unique meetings"** → the meeting WS isn't reaching the
  signaling server. Check the console for a blocked `ws://localhost:8080` (must be
  `wss://meet.localhost/lobby`) and that `/lobby` routes to `:8086`, not cc7 on `:8080`.
- **cc7 and videocall fight over `:8080`** → videocall's `websocket-api` must publish
  host `:8086` (`docker-compose.yaml`), leaving `:8080` for cc7's `dx serve`.
