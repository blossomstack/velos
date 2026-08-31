# Velos

**Velos** is a control plane for running containers across a pool of registered
worker machines, exposed over a RESTful API. You declare the containers you want;
Velos schedules them onto healthy workers, runs them through a container runtime,
and continuously reconciles their actual state back toward what you asked for.

The architecture is runtime- and OS-agnostic: workers talk to the control plane
over HTTP and execute containers through a pluggable runtime interface. The
current runtime backend is [Apple Containerization](https://github.com/apple/containerization)
(lightweight Linux micro-VMs); additional runtimes and platforms are a planned
direction.

```
   velosctl ─┐                  ┌──────────────────────────────┐
   (CLI)     │                  │          velos-server          │
   dashboard ├───  REST  ──────▶│  REST API · scheduler ·        │
   (browser) │   (Bearer)       │  reconciliation · web UI       │
             │                  │  SQLite-backed object store    │
             ▼                  └───────────────▲────────────────┘
                                                │ register · lease · status
                                      ┌─────────┴──────────┐
                                      │      veloslet       │  one per worker
                                      │   reconcile loop    │
                                      │  ContainerRuntime ──┼──▶ container runtime
                                      └─────────────────────┘
```

## Components

- **`velos-server`** — the control plane. Serves the REST API, persists objects
  in SQLite, runs the scheduler and reconciliation loops, and serves the web
  dashboard (embedded in the binary). Bind address and DB path are configurable
  via `--listen`/`VELOS_LISTEN` and `--db`/`VELOS_DB`.
- **`veloslet`** — the per-worker agent. Registers its machine, renews a lease to
  prove liveness, and reconciles its assigned containers against the runtime.
- **`velosctl`** — a command-line client for the API. `velosctl login` saves an
  admin token (and server URL) to `~/.velos/config` for subsequent calls, and
  `velosctl doctor` and `veloslet status` diagnose a setup that isn't working.
- **Web dashboard** — a React UI for first-run admin setup, watching workers and
  containers, launching workloads, and managing CLI tokens, served directly by the
  server.

## Resource model

Velos manages four object types, each with `metadata` / `spec` / `status`, served
under `/api/v1/{plural}`:

- **Container** — a workload. Its phase moves `Pending → Scheduled → Running →
  Succeeded | Failed`, or `Unknown` when its node's state is lost. It can also be
  parked in `Hibernated` — see [Hibernation](#hibernation).
- **Worker** — a registered machine, with its capacity and a `Ready` condition.
- **Lease** — a worker's periodic heartbeat; a stale lease marks its worker
  `NotReady`. A worker only renews it while its container runtime can actually
  run containers — see [Worker health](#worker-health).
- **Service** — a stable port in front of the containers a label selector picks
  out, wherever they are running — see [Services](#services).

## Services

A container's address lives on its worker's own container network, which is
reachable from that Mac and nowhere else. A **Service** turns a set of containers
into something the rest of the network can reach:

```bash
velosctl apply service -f web-service.json
```

```json
{
  "metadata": { "name": "web" },
  "spec": {
    "selector": { "app": "web" },
    "ports": [{ "targetPort": 8080 }]
  }
}
```

The server allocates a **node port** in `30000-32767`, and every worker running a
selected container opens that port and forwards it to `targetPort` inside the
container. `status.endpoints` lists where the service is answering right now:

```json
"endpoints": [
  { "workerName": "mac-1", "address": "192.168.68.51", "nodePort": 31007, "containerName": "web-1" }
]
```

Velos has no cluster IP, because there is nothing to put one on: every worker's
container network is a separate island and they all use the same address range.
So a Service is Kubernetes' `NodePort` with `externalTrafficPolicy: Local`, and
nothing else. The port only listens where a replica actually runs, which means an
external load balancer can point at every worker unconditionally — a worker
without a replica refuses the connection and drops out of rotation by itself.

Two consequences worth knowing before you use it:

- A node port is bound on `0.0.0.0` and is **not authenticated**. Anything on the
  LAN can reach it directly, bypassing whatever you put in front. This is true of
  Kubernetes NodePort too, and it is the reason to keep the workers on a trusted
  network.
- Forwarding is TCP and is done in userspace by `veloslet`. `container run
  --publish` would be the obvious mechanism and does not work: on
  apple/container 1.0.0 a published port binds and then fails to reach the
  container behind it.

See [the guide](docs/getting-started.md#9-exposing-a-service) for putting a real
reverse proxy in front of one.

## Hibernation

A container can be shut down temporarily without being destroyed:

```bash
velosctl hibernate my-job     # POST /api/v1/containers/my-job/hibernate
velosctl resume    my-job     # POST /api/v1/containers/my-job/resume
```

Hibernating stops the micro-VM but keeps everything else: the object, its
worker binding, its disk, and its share of that worker's capacity — so waking it
is guaranteed a slot and picks up the same instance rather than a fresh one.
It is not a delete, and it is not an exit: the `restartPolicy` does not apply to
a hibernated container.

Both endpoints are declarative and idempotent — they record the intent in
`spec.desiredState` (`Running` | `Hibernated`), and the owning worker converges
the micro-VM on its next reconcile pass and reports `status.phase`. A container
that has already finished (`Succeeded` / `Failed`) cannot be hibernated (`409`).

## Placement

By default the scheduler first-fits a container onto any ready worker with room.
A container's `spec` can constrain **where** it runs (Kubernetes-shaped):

- **`nodeName`** — pin to one worker by name.
- **`nodeSelector`** — require the worker to carry matching `metadata.labels`.
- **`affinity`** — richer node affinity: hard `required` terms (operators `In`,
  `NotIn`, `Exists`, `DoesNotExist`, `Gt`, `Lt`) and soft, weighted `preferred`
  terms that influence scoring.
- **`tolerations`** — allow scheduling onto workers whose `spec.taints`
  (`NoSchedule` / `PreferNoSchedule`) would otherwise repel the container.

The scheduler **filters** on the hard constraints, **scores** the survivors by the
soft preferences, and picks the best (ties break by input order, so an
unconstrained container behaves exactly like first-fit). A container that no
worker can satisfy stays `Pending` with a human-readable `status.message`. Once
bound, the placement is recorded in `status.workerName` and never re-evaluated.

```jsonc
// run only on GPU workers, preferring the "us" zone, tolerating the gpu taint
"spec": {
  "image": "…",
  "nodeSelector": { "gpu": "true" },
  "affinity": { "preferred": [
    { "weight": 50, "preference": { "matchExpressions": [
      { "key": "zone", "operator": "In", "values": ["us"] } ] } } ] },
  "tolerations": [ { "key": "gpu", "operator": "Exists" } ]
}
```

## Worker health

A worker's lease is a claim that its machine can run containers, so `veloslet`
checks that before every heartbeat rather than assuming it. The `container` CLI
being installed is not the same question: Apple's container services are a
launchd job that a reboot, a crash, or `container system stop` can take away
while the CLI keeps answering `--version` quite happily.

So on each heartbeat the worker asks the runtime to list its containers, and:

- if that works, it renews its lease as usual;
- if it does not, the worker restarts the runtime's services and tries again;
- if the runtime still cannot answer, the worker **stops renewing** and goes
  `NotReady`. The scheduler then places nothing new on it, and its containers are
  rescheduled once it has been `NotReady` past the eviction window.

`veloslet status` runs the same check, so a machine whose runtime is down reports
a failed `runtime` line and exits non-zero instead of a clean bill of health.

## Getting started

Install the client tools:

```bash
curl -fsSL https://raw.githubusercontent.com/blossomstack/velos/main/install.sh | sh
```

That downloads the latest release for your platform, checks it against its published
SHA-256, and installs `velosctl` (drive the control plane) and `veloslet` (join it as
a worker) into `~/.local/bin`. Useful flags: `--components all` (also install
`velos-server`), `--components velosctl` (just the CLI), `--bin-dir /usr/local/bin`,
`--version v0.4.0`. Prefer cargo? `cargo install velos-server velosctl veloslet`.
Or build from source with `make build` (which also builds the embedded dashboard).

At any point, **`velosctl doctor`** checks your setup — config file, server URL,
credential, whether the control plane is reachable and initialized, how many
workers are Ready, and whether this machine has a container runtime — and prints
what to fix. On a worker machine, **`veloslet status`** does the same for the
worker: whether it has joined, whether its credential is still accepted, whether
its advertised capacity fits the host, and whether the background agent is running.

Then follow **[docs/getting-started.md](docs/getting-started.md)** for the full
walkthrough: start the control plane, set up the admin account and connect
`velosctl`, register a worker, launch containers, and open the dashboard at
`http://127.0.0.1:8080`. On first run the dashboard prompts you to create the
admin account; from there you mint a CLI token for `velosctl`. (Running a worker
currently requires the Apple `container` CLI; the control plane, CLI, and
dashboard do not.)

## Development

```bash
make build        # build the web UI + workspace
make web          # rebuild just the web UI (embedded by the server)
make test         # cargo test --workspace
make test-install # end-to-end test of install.sh
make check        # fmt + clippy + test + install.sh test  (pre-PR gate)
make run          # run the server
make install-ctl  # install velosctl into ~/.cargo/bin
make install-let  # install veloslet into ~/.cargo/bin
```

Engineering conventions and the design philosophy live in [`CLAUDE.md`](CLAUDE.md).

## License

MIT — see [LICENSE](LICENSE).
