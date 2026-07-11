# Worker system info + join-token management — design

**Date:** 2026-07-11

Two independent, dashboard-facing features shipped together:

- **A. Richer worker info** — report and display the veloslet agent version, OS,
  CPU arch, and hostname for each worker (today only the container-runtime
  version is known).
- **B. Join-token management** — list and manually revoke the bootstrap (join)
  tokens that workers use to register, so a leaked token can't be abused to
  register rogue workers indefinitely (auto-expiry via TTL already exists).

Plus a small correctness fix: **deleting a worker also revokes its per-worker
credential**, so a removed worker can't keep heartbeating / re-registering.

## Background (current state)

- A worker registers via `POST /auth/v1/register` presenting a **bootstrap
  token** (admin-minted, default 24h TTL). The server verifies it and mints a
  long-lived **worker credential** (`{worker}.{secret}`, no expiry) that the
  agent reuses for every heartbeat/API call.
- Registration sends `name`, `capacity`, `addresses` (currently `[]`), and
  `containerRuntimeVersion`. The veloslet binary version, OS, arch, and hostname
  are **not** sent. `crates/veloslet/src/host.rs::detect_host()` already reads
  host facts via macOS `sysctl`.
- Bootstrap tokens store only `secretHash` + `expiresAt`; they are **not
  listable and not revocable** (only expire). Admin/CLI tokens already have full
  list/create/revoke (`crates/auth` `list_admin_tokens`/`revoke_admin_token`,
  `/auth/v1/admin/tokens`, `web/src/views/Tokens.tsx`) — the exact pattern to
  mirror.
- `revoke_credential(worker)` exists in `crates/auth` but is not wired to any
  handler. The generic `delete` handler removes the object but leaves the
  credential.
- fluorite `velos_models` types are the wire/OpenAPI contract only; **no Rust
  code consumes them** (server/veloslet use raw `serde_json::Value`), so
  extending `.fl` cannot break typed constructors.
- Last-heartbeat is **already** surfaced in the dashboard (`Workers.tsx` reads
  leases → "lease renewed X ago"); it stays derived from the lease (no model or
  write-churn change).

## Feature A — Worker system info

### Model (`crates/models/fluorite/velos.fl`)

Add a nested block on `WorkerStatus` (mirrors k8s `status.nodeInfo`):

```
struct NodeSystemInfo {
    agent_version: String,   // veloslet CARGO_PKG_VERSION
    os: String,              // e.g. "macOS 15.1"
    arch: String,            // e.g. "arm64"
    hostname: String,
}
```

- `WorkerStatus` gains `node_info: NodeSystemInfo`.
- `RegisterRequest` gains `node_info: NodeSystemInfo`.

This is contract/documentation only (no Rust consumer today), but keeps the wire
contract honest.

### veloslet (`crates/veloslet`)

- `host.rs`: add `detect_system_info() -> SystemInfo` (side-effecting edge, thin,
  untested like `detect_host`). Read `kern.osproductversion`, `hw.machine`,
  `kern.hostname` via the existing `sysctl` helper; agent version from
  `env!("CARGO_PKG_VERSION")`. Compose `os` as `"macOS <osproductversion>"`.
  Each field falls back to `"unknown"` / empty on failure (never abort
  registration over cosmetic facts).
- `main.rs::run`: include `nodeInfo` in the registration JSON body.

### server (`crates/server` `register`)

Copy `nodeInfo` from the request into the worker `status` document, defaulting to
an all-`"unknown"` block when an older agent omits it. Returned as-is by
`GET /api/v1/workers`.

### web (`crates/velos` `web/`)

- `types.ts`: add `NodeSystemInfo` and `WorkerStatus.nodeInfo?`.
- `Workers.tsx`: show agent version on the card subtitle (alongside runtime),
  and add Agent version / OS / Arch / Hostname / **Last seen** fields to
  `WorkerDrawer`. "Last seen" reuses the lease renewTime already fetched.

## Feature B — Join-token management

Bootstrap tokens stay a **separate token kind** (distinct identity semantics from
admin tokens; keeps `authenticate()` clean).

### auth (`crates/auth`)

- On `mint_bootstrap_token`, also persist `label` (optional, may be empty) and
  `createdAt` alongside `secretHash`/`expiresAt`. Signature becomes
  `mint_bootstrap_token(label: &str, ttl_secs: i64)`.
- Add `BootstrapTokenSummary { id, label, created_at, expires_at }` (plain
  struct, mirrors `AdminTokenInfo`; no secret).
- Add `list_bootstrap_tokens() -> Vec<BootstrapTokenSummary>` and
  `revoke_bootstrap_token(id)` (deletes the `BootstrapToken` row →
  `verify_bootstrap` then fails closed).

### server (`crates/server`)

Extend the existing `/auth/v1/tokens` collection (admin-only):

- `POST /auth/v1/tokens` — mint; accept an optional `label` in the body (keeps
  `ttlSeconds`, default 24h).
- `GET /auth/v1/tokens` — list summaries.
- `DELETE /auth/v1/tokens/{id}` — revoke.

### web

- `api.ts`: `useJoinTokens` / `useCreateJoinToken` / `useRevokeJoinToken` hitting
  `/auth/v1/tokens`.
- `Tokens.tsx`: add a "Join tokens" card (create with label + TTL; the one-time
  `{id}.{secret}` shown once with the `veloslet --token` hint; list id/label/
  created/expires, flag expired; revoke). Keep the existing CLI-token card.

## Delete-worker revokes credential

In the `delete` handler, when `kind == "Worker"` and the object is actually
removed (the non-finalizer hard-delete branch — workers carry no finalizers),
call `auth.revoke_credential(name)`. Best-effort: a revoke error is logged, not
fatal to the delete. `app()` (no-auth dev/test build) has no auth service, so
this is a no-op there.

## Testing

- **auth** (unit, alongside source): bootstrap mint stores label/createdAt;
  `list_bootstrap_tokens` returns them without secrets; `revoke_bootstrap_token`
  makes `verify_bootstrap` fail closed. Extend the existing bootstrap test.
- **server** (`velos-tests` e2e): register round-trips `nodeInfo` into
  `GET /workers`; `DELETE /workers/{name}` then rejects the old worker
  credential; `GET`/`POST`/`DELETE /auth/v1/tokens` are admin-gated and
  round-trip a token through list → revoke.
- **veloslet**: detection stays a thin untested edge (like `detect_host`); no new
  pure logic to unit-test.
- **web**: `npm run build` stays green (CI `web` job).

## Gate (must be green for the PR)

CI runs two jobs: `check` (`cargo fmt --all --check`; `cargo clippy --all-targets
--all-features -D warnings`; `cargo test --workspace`) and `web` (`npm ci &&
npm run build`). Clippy is strict: no `unwrap`/`expect`/`panic`/wildcard match in
production code.

## Out of scope

- Worker↔token association / attribution (explicitly dropped).
- Cascade revoke (revoking a join token does not deauthorize already-joined
  workers — they hold their own credential; use worker delete for that).
- Single-use / use-count-limited bootstrap tokens.
- Last-used tracking on any token.
