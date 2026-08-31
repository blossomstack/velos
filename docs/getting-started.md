# Getting started with Velos

This guide walks through running Velos end-to-end: install it, start the control
plane, set up the admin account, connect `velosctl`, register a worker, launch
containers, and understand what happens under the hood.

- [1. Prerequisites](#1-prerequisites)
- [2. Install](#2-install)
- [3. Start the control plane](#3-start-the-control-plane)
- [4. First-run setup & connecting velosctl](#4-first-run-setup--connecting-velosctl)
- [5. Register a worker](#5-register-a-worker)
- [6. Use the CLI (velosctl)](#6-use-the-cli-velosctl)
- [7. Use the dashboard](#7-use-the-dashboard)
- [8. The container lifecycle](#8-the-container-lifecycle)
- [9. Exposing a service](#9-exposing-a-service)
- [10. Authentication](#10-authentication)
- [11. Troubleshooting](#11-troubleshooting)
- [12. Tearing down](#12-tearing-down)

---

## 1. Prerequisites

| Requirement | Needed for |
|---|---|
| **curl** + **tar** | Installing the prebuilt binaries with the install script (§2). |
| **Rust** (stable) | Building from source. Pinned by `rust-toolchain.toml`. |
| **Node.js 18+** + npm | Building the dashboard from source (not needed if you `cargo install`). |
| **Apple `container` CLI** | Running a **worker** — this is the current container runtime backend. The control plane, CLI, and dashboard don't need it. |
| **jq** | Used by the token-minting snippet below. |

The control plane and clients are runtime-agnostic; only the worker executes
containers, and today it does so through Apple Containerization. Check the
runtime on a machine that will host workloads:

```bash
container --version
```

## 2. Install

### Via the install script (prebuilt binaries)

```bash
curl -fsSL https://raw.githubusercontent.com/blossomstack/velos/main/install.sh | sh
```

This picks the release build for your platform (macOS or Linux, arm64 or x86_64),
verifies it against its published SHA-256, and installs `velosctl` and `veloslet`
into `~/.local/bin`. It prints an `export PATH=…` line if that directory isn't on
your `PATH` yet. To pass options through the pipe, add `-s --`:

```bash
# add the control plane too, installed system-wide
curl -fsSL https://raw.githubusercontent.com/blossomstack/velos/main/install.sh \
  | sh -s -- --components all --bin-dir /usr/local/bin
```

| Option | Env | Default | Meaning |
|---|---|---|---|
| `--components` | `VELOS_COMPONENTS` | `velosctl,veloslet` | comma-separated `velosctl`, `veloslet`, `velos-server`, or `all` |
| `--bin-dir` | `VELOS_BIN_DIR` | `~/.local/bin` | where to install |
| `--version` | `VELOS_VERSION` | latest release | release tag, e.g. `v0.1.3` |

The script fails closed: an unsupported platform, a missing or mismatched
checksum, or a binary that won't run here all abort the install instead of
leaving something broken on your `PATH`. Re-running it upgrades in place.

### Via cargo

```bash
cargo install velos-server   # control plane (the web dashboard is built in)
cargo install velosctl          # CLI
cargo install veloslet          # worker agent
```

### From source

```bash
git clone https://github.com/blossomstack/velos
cd velos
make build      # builds the web UI, then all binaries into target/debug/
```

To put the CLIs on your `PATH` (release builds into `~/.cargo/bin`):

```bash
make install-ctl    # install velosctl
make install-let    # install veloslet (the worker agent)
```

The rest of this guide uses bare command names (`velos-server`, `velosctl`,
`veloslet`); if you built from source without installing, run them from
`./target/debug/` or add that directory to your `PATH`.

## 3. Start the control plane

```bash
velos-server
```

- Listens on **`127.0.0.1:8080`** and serves both the API and the **web
  dashboard** (open `http://127.0.0.1:8080`).
- Creates a SQLite database **`velos.db`** in the working directory.
- Runs the scheduler (every ~2s) and the worker-health controller (every ~5s).

The bind address and database path are configurable:

| Setting | Flag | Env | Default |
|---|---|---|---|
| Listen address | `--listen` | `VELOS_LISTEN` | `127.0.0.1:8080` |
| Database path | `--db` | `VELOS_DB` | `velos.db` |

```bash
velos-server --listen 0.0.0.0:8080 --db /var/lib/velos/velos.db
# or via env:
VELOS_LISTEN=0.0.0.0:8080 velos-server
```

> Binding `0.0.0.0` exposes the server on the network. That's reasonable now that
> auth is enforced (§10), but anyone who can reach the port can still attempt the
> first-run setup — initialize the admin account promptly.

Control log verbosity with `RUST_LOG`, e.g. `RUST_LOG=info velos-server`.

A freshly started server is **uninitialized** and fails closed: every route
except the first-run setup is rejected until you create the admin account
(§4). Leave the server running and open a new terminal for the next steps.

## 4. First-run setup & connecting velosctl

Velos has one **admin** account, created once on first run, plus per-worker
identities (§10). The admin is set up through the dashboard, which then mints the
**CLI token** that `velosctl` carries.

1. Open **`http://127.0.0.1:8080`**. On first run it shows a **Setup** screen —
   choose an admin username and password. (The password is hashed with argon2 and
   never leaves the server; setup works only while the server is uninitialized.)
2. You're signed in. Go to the **Tokens** tab → **Create CLI token**, give it a
   label (e.g. `laptop`), and **copy the token — it is shown only once.**
3. Hand that token to `velosctl`:

```bash
velosctl login --token <PASTE_TOKEN> --server http://127.0.0.1:8080
```

`login` validates the token against the server, then saves the **server and
token** to `~/.velos/config` (mode `0600`). After this, plain commands need no
flags:

```bash
velosctl get workers     # uses the saved server + token
velosctl logout          # forget the saved credential
```

Resolution precedence, highest first:

| Value | Order |
|---|---|
| token | `--token` flag → `VELOS_TOKEN` env → `~/.velos/config` |
| server | `--server` flag → `VELOS_SERVER` env → `~/.velos/config` → `http://127.0.0.1:8080` |

### Check your setup with `velosctl doctor`

```bash
velosctl doctor
```

It reports, in order: the `velosctl` version and where it is installed; the config
file and its permissions; the server URL **and which layer supplied it**; whether a
credential is present; whether the server answers, is initialized, and accepts that
credential; how many workers are Ready; and whether this machine has the `container`
runtime. Every line that isn't a pass carries the command that fixes it.

Run against a server that is up but not set up yet, it looks like this:

```
  ✔ velosctl     v0.1.3 (/Users/you/.local/bin/velosctl)
  ! config file  /Users/you/.velos/config not written yet
                   → run `velosctl login --token <token> --server <url>` to save one
  ✔ server url   http://127.0.0.1:8080 (from built-in default)
  ✗ credential   none — every API call will be rejected
                   → run `velosctl login --token <token> --server <url>`
  ✔ reachable    http://127.0.0.1:8080 answered /healthz
  ✗ initialized  no admin account — the server rejects everything until first-run setup
                   → open http://127.0.0.1:8080 and create the admin account
  - identity     no credential to check
  - api access   no credential to check
  ✔ runtime      container CLI version 1.0.0

2 failed, 1 warning — see the hints above
```

A `-` means the check was skipped because an earlier one made it meaningless — a
server that isn't answering can't tell you whether your token is good. `doctor`
exits non-zero when something failed (warnings alone still exit 0), so a setup
script can gate on it. It only reads; nothing it does changes state.

> Prefer the CLI without a browser? You can drive setup over HTTP directly:
> `curl -X POST :8080/auth/v1/setup -d '{"username":"admin","password":"…"}'`,
> then `curl -X POST :8080/auth/v1/login …` for a session token and
> `POST /auth/v1/admin/tokens {"label":"laptop"}` for a CLI token. See §10.

## 5. Register a worker

Worker registration is a fail-closed, two-step flow: an **admin** mints a
short-lived *join token* (also called a bootstrap token), then `veloslet`
exchanges it for a durable, node-scoped worker credential on first start.

The exchange is **one-shot**. Once a worker holds a credential it presents that
on every later start and never uses the join token again, so a join token that
has expired (or been revoked) stops *new* workers joining without disturbing any
worker already in the fleet. Revoking a worker's credential — which is what
deleting the worker does — is the way to evict one for good.

Joining happens exactly once, in `veloslet setup`. It is the only command that
can mint a credential, so a join token never reaches the config file or the
process table.

```bash
# As the logged-in admin, mint a bootstrap token and assemble it as `tokenId.secret`.
TOKEN=$(velosctl token create | jq -r '"\(.tokenId).\(.secret)"')

# On the worker machine: join, and write ~/.velos/veloslet.json.
veloslet setup --server http://127.0.0.1:8080 --node "$(hostname -s)" --token "$TOKEN" \
  --cpu 8 --memory 16G

# Then run the worker. It re-registers on start and renews its lease.
veloslet run
```

`veloslet setup` flags. Only `--token` is always required: **every other setting
falls back to the existing config**, so re-joining a machine that has been set up
before is just

```bash
veloslet setup --token "$TOKEN"
```

| Flag | Default | Meaning |
|---|---|---|
| `--token` | *(required)* | join token, traded for a credential here and never written to disk |
| `--server` | existing config | control-plane base URL |
| `--node` | existing config | this worker's unique name |
| `--cpu` | existing config | advertised CPU cores; must not exceed the machine's |
| `--memory` | existing config | advertised memory, e.g. `16G`; must not exceed the machine's |
| `--reconcile-secs` | existing config, else `5` | how often it reconciles its containers |
| `--heartbeat-secs` | existing config, else `10` | how often it renews its lease |
| `--lease-secs` | existing config, else `40` | lease duration; not renewed in time → worker goes `NotReady` |
| `--config` | `~/.velos/veloslet.json` | which config to read and write |

The four settings above the intervals are required only when there is no config
yet, and a first run reports all the missing ones at once rather than one per
attempt. A config file that exists but cannot be parsed is an error rather than
a fresh start, so `setup` never silently overwrites a file it could not read.

Nothing is written unless the join succeeds, so a failed `setup` leaves the
machine exactly as it was — there is no half-joined config for `run` to puzzle
over.

Before each heartbeat the worker checks that its container runtime can actually
run containers, and restarts Apple's `container` services if they are not up. A
runtime it cannot bring back means it stops renewing its lease and goes
`NotReady`, so the scheduler stops sending it work rather than filling it with
containers that would never launch. `veloslet status` reports the same thing.

Within a few seconds the worker reports **Ready** (its lease is fresh):

```bash
velosctl get workers
```

### Reading and changing the config

`veloslet config` edits the file `setup` wrote. Fields are named, not free-form
keys, so a typo is rejected up front instead of landing in the file:

```bash
veloslet config show               # whole config as JSON, credential redacted
veloslet config get cpu            # one field
veloslet config set --cpu 12 --memory 24G
veloslet config path               # where the file is
```

Settable fields are `server`, `node`, `cpu`, `memory`, `reconcile-secs`,
`heartbeat-secs` and `lease-secs`. Two things `config` will not do: print or set
the **credential** (it is earned by `setup`, not declared — `show` redacts it),
and **rename a worker that has already joined**, because its credential is bound
to the name the server issued it for. Changing `--cpu`/`--memory` is validated
against the host straight away, so an impossible value fails while you are still
looking at the terminal rather than at the next restart.

Restart the worker for a change to take effect.

### Check a worker with `veloslet status`

```bash
veloslet status
```

The worker-side counterpart of `velosctl doctor`, named for what it reports. It
gives the `veloslet`
version and path; the config file and its permissions; whether this worker has
joined; the server URL and whether it answers; whether the server still accepts
this worker's credential **and agrees on its name**; advertised capacity against
what the machine physically has; the `container` runtime; and whether launchd is
running the background worker. Every line that isn't a pass carries the command
that fixes it.

```
  ✔ veloslet     v0.3.0 (/Users/you/.local/bin/veloslet)
  ✔ config file  /Users/you/.velos/veloslet.json
  ✔ joined       holds a credential for macmini-2
  ✔ server       http://127.0.0.1:8080
  ✔ reachable    http://127.0.0.1:8080 answered /healthz
  ✔ identity     the server knows this worker as macmini-2
  ✔ capacity     advertising 8 cpu, 16G of 10 cpu, 32G
  ✔ runtime      container CLI version 1.0.0
  ! background   no LaunchAgent loaded — this worker only runs while `veloslet run` is in a terminal
                   → run `veloslet run -d` to keep it running across logins and crashes

1 warning, nothing broken
```

Like `velosctl doctor` it only reads, a `-` means the check was skipped because
an earlier one made it meaningless, and it exits non-zero when something failed
(warnings alone still exit 0). Two failures it is specifically built to name,
because both otherwise surface as an unexplained `401` in the worker's log: a
credential the server has revoked (the worker was deleted), and a config whose
`node` was edited after joining, so the credential belongs to a different name.

### Run as a background daemon

`veloslet run -d` runs the same worker as a long-running service (a launchd
**LaunchAgent** on macOS) so it starts at login and restarts on crash:

```bash
veloslet run -d
```

It uses the config `setup` already wrote, so it takes no server, node or token
flags at all — and it refuses to start a worker that has not joined, rather than
leaving a background process that cannot authenticate. Capacity is re-validated
against the host here too.

The agent runs `veloslet run --config ~/.velos/veloslet.json`. Logs go to
`~/Library/Logs/veloslet.{out,err}.log`. Two ways to take it away:

```bash
veloslet stop         # unload the agent; bundle and config stay, `run -d` restarts it
veloslet uninstall    # remove the agent, the app bundle, and the config for good
```

`uninstall` deletes the credential along with the config, so rejoining afterwards
needs a fresh join token and another `veloslet setup`. Use `stop` if you only
want the worker to stand down for a while.

> **macOS Local Network privacy.** A bare launchd agent is silently blocked from
> reaching a server on your LAN, because it has no GUI app for macOS to attribute
> the connection to. To work around this (per Apple TN3179), `run -d` wraps the
> binary in a small code-signed app bundle (`~/Applications/Velos.app`) with a
> bundle identifier and an `NSLocalNetworkUsageDescription`, and references it from
> the agent via `AssociatedBundleIdentifiers`. The first time it connects, macOS
> shows a **"Velos Worker wants to access your local network"** prompt — **approve
> it** (or enable *Velos Worker* under System Settings → Privacy & Security → Local
> Network). Until then the worker can't reach the server.

## 6. Use the CLI (velosctl)

Once you've run `velosctl login` (§4), commands carry your admin credential
automatically — no `--token` needed.

```bash
# List / get
velosctl get workers
velosctl get containers
velosctl get container my-job
velosctl get containers --selector app=demo
velosctl get services

# Create from a JSON file (status.phase MUST be "Pending" to be scheduled)
cat > job.json <<'JSON'
{
  "metadata": { "name": "my-job", "labels": { "app": "demo" } },
  "spec": {
    "image": "docker.io/library/alpine:latest",
    "command": ["sleep", "600"],
    "resources": { "cpu": 1, "memoryBytes": 268435456 },
    "restartPolicy": "Never"
  },
  "status": { "phase": "Pending" }
}
JSON
velosctl apply container --file job.json

# Suspend / wake (see §8)
velosctl hibernate my-job
velosctl resume my-job

# Delete
velosctl delete container my-job

# Diagnose the setup: config, server, credential, workers, runtime
velosctl doctor
```

> **Why `status.phase: "Pending"`:** the scheduler only places containers whose
> phase is `Pending`. The dashboard sets this for you.

For a one-off against a different server or with a different identity, override
per-command: `velosctl --server http://other:8080 --token <tok> get workers`.

## 7. Use the dashboard

The dashboard is served by the server — just open **`http://127.0.0.1:8080`**.
After signing in (§4) it gives you:

- **Overview** — workers ready, container counts, cluster CPU/memory allocation
  (counted over **Ready** workers only, since nothing can be scheduled onto a
  NotReady one), and a containers-by-phase breakdown.
- **Workers** — per-node cards (Ready status, runtime version, live allocation,
  slot usage, lease freshness) with a detail drawer.
- **Containers** — a phase-filterable table with a **Launch container** form,
  per-row delete, and a detail drawer.
- **Tokens** — create, list, and revoke the CLI tokens that `velosctl` uses.

Data refreshes every 2 seconds. **Sign out** from the header clears the browser
session. To iterate on the UI itself, run the Vite dev server (it proxies the API
to the server for hot-reload):

```bash
cd web && npm install && npm run dev      # http://localhost:5173
```

## 8. The container lifecycle

1. **`Pending`** — created via the API with `status.phase: Pending`.
2. **`Scheduled`** — the scheduler binds it to a Ready worker with capacity and
   sets `spec.nodeName`.
3. **`Running`** — that worker's `veloslet` starts the container via the runtime
   and reports its ID.
4. **`Succeeded` / `Failed`** — when the process exits (0 vs non-zero); the
   `restartPolicy` (`Never` / `OnFailure` / `Always`) decides whether it restarts.

**`Hibernated`** sits outside that line. Hibernating shuts the micro-VM down
while keeping the container object, its worker binding, its disk, and its slice
of that worker's capacity:

```bash
velosctl hibernate my-job     # POST /api/v1/containers/my-job/hibernate
velosctl resume    my-job     # POST /api/v1/containers/my-job/resume
```

Both are declarative: they set `spec.desiredState` (`Running` | `Hibernated`) and
the owning `veloslet` converges the micro-VM on its next pass, then reports
`status.phase`. So the phase lags the call by a reconcile interval — poll until
it reads `Hibernated`. Repeat calls are no-ops, `resume` boots the *same*
instance back up (its disk survives), and a container that has already finished
cannot be hibernated (`409`). A hibernated container keeps its reservation, so
waking it never has to compete for its worker's capacity.

If a worker's lease goes stale, the health controller marks it `NotReady`; after
a grace period its containers are evicted (rescheduled if labeled
`velos.io/reschedulable=true`, otherwise marked `Unknown`).

## 9. Exposing a service

A container's address (`192.168.64.x` under Apple Containerization) is reachable
from its own worker Mac and nowhere else, and every worker hands out the same
range. A **Service** is how you get a container to answer somewhere the rest of
your network can reach.

### Create one

Label the container, then select it:

```bash
cat > web.json <<'JSON'
{
  "metadata": { "name": "web-1", "labels": { "app": "web" } },
  "spec": {
    "image": "docker.io/library/nginx",
    "resources": { "cpu": 1, "memoryBytes": 536870912 }
  },
  "status": { "phase": "Pending" }
}
JSON

cat > web-svc.json <<'JSON'
{
  "metadata": { "name": "web" },
  "spec": {
    "selector": { "app": "web" },
    "ports": [{ "targetPort": 80 }]
  }
}
JSON

velosctl apply container -f web.json
velosctl apply service   -f web-svc.json
velosctl get service web
```

The server fills in a **node port** from `30000-32767`:

```json
"ports": [{ "targetPort": 80, "nodePort": 31007 }]
```

Give it a reconcile interval, and `status.endpoints` tells you where it is
answering:

```json
"endpoints": [
  { "workerName": "mac-1", "address": "192.168.68.51", "nodePort": 31007, "containerName": "web-1" }
]
```

`curl http://192.168.68.51:31007` now reaches the container.

Set `nodePort` yourself if you want a fixed one; the server rejects it with `409`
if another Service already holds it. Multiple ports each need a `name`.

### What is actually happening

Every worker running a selected container opens the node port on `0.0.0.0` and
forwards it, in userspace, to `targetPort` inside the container. This is
Kubernetes' `NodePort` with `externalTrafficPolicy: Local`: the port listens
**only** where a replica is running. A worker without one refuses the connection.

That property is what makes the next step simple.

### Put a reverse proxy in front

Because a worker without a replica refuses the connection, a proxy can list every
worker unconditionally and let health checks decide. With Caddy:

```caddyfile
web.example.lan {
	reverse_proxy 192.168.68.51:31007 192.168.68.52:31007 192.168.68.53:31007 {
		lb_policy       round_robin
		health_uri      /
		health_interval 5s
	}
}
```

Move the container to another worker and Caddy follows within one health
interval — no config change, because the node port is the same number on every
worker. The worker list only changes when you add or remove machines.

For nginx the equivalent is an `upstream` block with `max_fails` / `fail_timeout`;
for HAProxy, a `backend` with `check`.

### Things to know

- **Node ports are unauthenticated.** They are bound on `0.0.0.0`, so anything on
  the LAN can reach a container directly and bypass your proxy. Kubernetes
  NodePort behaves the same way. Keep workers on a trusted network, or firewall
  the range to the proxy's address.
- **TCP only.** The worker proxy forwards TCP. UDP is not supported.
- **A worker that advertises no address publishes no endpoint.** `veloslet` reports
  the address it reaches the control plane from when it registers; if it cannot
  work one out, its containers still run but never appear in `status.endpoints`,
  and the server logs why.
- **`container run --publish` is not used**, and cannot be: on apple/container
  1.0.0 a published port binds on the host and then fails to reach the container
  behind it (`backend - connect failed: No route to host`), so connections are
  accepted and dropped. Velos does the forwarding itself instead.

## 10. Authentication

Velos is fail-closed and recognizes two kinds of identity:

- **Admin** — full access to all resources and to the privileged auth endpoints.
  There is one admin account (username + argon2-hashed password), created once
  via first-run setup.
- **Worker** — a registered machine. A worker credential can read all
  workers/containers/leases and manage containers, but may only address its *own*
  Worker/Lease object by name.

**Initialization gate.** Until the admin account exists, the server is
*uninitialized*: only `GET /auth/v1/status` and `POST /auth/v1/setup` are
reachable; everything else returns `401`. `setup` is single-shot — once an admin
exists it returns `409`.

**Admin tokens.** Both the dashboard session and `velosctl`'s credential are the
same primitive: a random opaque token, persisted only as a hash and looked up on
each request. Logging in returns a short-lived **session token** (held by the
browser); the **Tokens** page mints long-lived **CLI tokens** (the GitHub
personal-access-token model). Revoking a token in the dashboard takes effect
immediately. `velosctl login` stores its token + server in `~/.velos/config`
(`0600`).

**Worker credentials.** An admin mints a join token (`POST /auth/v1/tokens`);
`veloslet` exchanges it (`POST /auth/v1/register`) for a durable
`workerName.secret` credential, and the server creates the `Worker` object.

`register` also accepts a worker's own credential, which is how a restarting
worker republishes its capacity and system info. That call mints nothing — only a
join token does — so the join is one-shot and a worker can never be stranded by an
expired one. A worker may only register under its own name; another worker's
credential, or an admin token, is rejected with `403`.

Auth endpoints at a glance:

| Endpoint | Who | Purpose |
|---|---|---|
| `GET /auth/v1/status` | open | `{ "initialized": bool }` |
| `POST /auth/v1/setup` | open *(uninitialized only)* | create the admin account |
| `POST /auth/v1/login` | open | username+password → session token |
| `GET /auth/v1/me` | any valid token | echo the caller's identity |
| `GET/POST /auth/v1/admin/tokens`, `DELETE …/{id}` | **admin** | list / create / revoke CLI tokens |
| `POST /auth/v1/tokens` | **admin** | mint a worker join (bootstrap) token |
| `POST /auth/v1/register` | join token *or* the worker's own credential | join → worker credential; re-register → refresh only |

> Identity is resolved behind a `TokenVerifier` seam, so an external OIDC provider
> can be integrated later (validate a JWT against the provider) without changing
> any endpoint. Single-admin and the two-tier model are the current scope.

## 11. Troubleshooting

Start with **`velosctl doctor`** (§4), or **`veloslet status`** on a worker machine — it names the broken layer and the command
that fixes it. The table below covers the rest.

| Symptom | Likely cause / fix |
|---|---|
| `{"error":"unauthorized"}` from `/api/v1/*` | Not logged in — run `velosctl login` (§4) — or the token was revoked/expired, or the server isn't set up yet (`GET /auth/v1/status`). |
| `401` on everything, even `/auth/v1/login` | Server is **uninitialized**; complete first-run setup in the dashboard (§4). |
| `409` from `/auth/v1/setup` | The admin already exists; use **login**, not setup. |
| `velosctl token create` → `403`/`401` | Bootstrap minting is **admin-only**; log in first (§4). |
| Container stuck in `Pending` | Created without `status.phase: "Pending"`, or no worker is `Ready` / has capacity. |
| Container goes straight to `Failed` | The runtime couldn't run it (image pull failed, or the `container` CLI is missing on the worker). Check the `veloslet` logs. |
| Worker shows `NotReady` | `veloslet` isn't renewing its lease — confirm it's running and can reach the server. |
| Daemon (`veloslet run -d`) logs `error sending request` and never registers | On macOS, the **Local Network** prompt wasn't approved — enable *Velos Worker* under System Settings → Privacy & Security → Local Network (see §5). Note: this grant can't be reset with `tccutil`; it persists per bundle id even after the app is deleted. |
| Worker logs `register failed, retrying in 10s: server returned 401` | Its credential is no longer accepted — the worker was deleted, which revokes it. Mint a new join token and run `veloslet setup` again. (A join token expiring cannot cause this: `setup` consumes it and a running worker never presents one.) |
| Dashboard says "server unreachable" | The server isn't running, or you opened the dev server while the server is down. |
| `address already in use` on start | Something already holds `:8080` — `lsof -nP -iTCP:8080 -sTCP:LISTEN`. |

## 12. Tearing down

```bash
# Stop the processes (Ctrl-C in their terminals, or:)
pkill -f velos-server
pkill -f veloslet

# If the worker was installed as a daemon (macOS), remove the LaunchAgent:
veloslet uninstall

# Forget the saved CLI credential
velosctl logout

# Reset all control-plane state (including the admin account)
rm -f velos.db
```
