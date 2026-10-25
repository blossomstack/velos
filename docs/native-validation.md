# Native workload implementation verification

Date: 2026-10-25. Host: `macmini-2.local`, ARM64 macOS.
Base: `ae1296a0ea962b7c4131bfdd803aa32278e19e56` from `blossomstack/velos`.
Branch: `feat/native-mlx-runtime`. Local implementation only.

## Passing checks

- `RUSTC_WRAPPER= cargo fmt --all --check`
- `RUSTC_WRAPPER= cargo clippy --all-targets --all-features --offline -- -D warnings`
- `RUSTC_WRAPPER= cargo test --workspace --offline -- --skip real_container_lifecycle_with_apple_containerization --test-threads=1`
  — **236 passed, zero failed**, one fixture entrypoint intentionally ignored,
  one real Apple Containerization test explicitly filtered out.
- `sh scripts/test-install.sh` — nine installer fixture cases passed; fake local
  release files and temporary output directories, no installed Velos service.
- `git diff --check`

The native integration target passes all ten tests, also in parallel. Coverage:
owned lifecycle/idempotency; unrelated-process isolation; graceful/forced stop and
Drop cleanup; foreground process trees; exit codes; literal arguments/environment
and working directory; bounded log draining; duplicate-profile rejection; exclusive
runtime ownership; stale-record refusal; missing executable and retry backoff;
mixed container/native dispatch; loopback readiness with matching PID; and real
API/scheduler/reconciler hibernate/resume/delete. No Python, MLX or model weights
are loaded by these tests.

## Findings and limits

Darwin's group-signal implementation can return EPERM when the only member is an
exited, unreaped child. Initially this looked like a permission boundary, including
on an escalated run. A separate read-only group probe plus Apple's kernel source
identified the zombie-only case. The runtime now accepts that case only after
WNOWAIT exit evidence and a successful group query showing no other member;
real permission failures still fail closed. Native tests pass with this correction.

The existing proxy rebind test failed in the parallel workspace run and passed
in isolation and the serial run. Full default-parallel `make check` is therefore
**not claimed green**. Initial existing proxy regression fixtures briefly used
production wildcard test listeners; those fixtures were changed to loopback and
an ephemeral port before the final validation. Production NodePort behavior remains
unchanged. No persistent listeners, remote routes or security settings were configured.

No real container runtime, MLX/Metal workload, existing audio service or mini 1 was
started, stopped or changed. Real-model startup, peak memory, inference, drain,
readiness adaptation and rollback need a separately coordinated integration run.
The POC's liveness-only `/health` does not satisfy the optional `ready:true` contract;
readiness remains unknown until the application exposes it. No persistent service
registration, deployment, merge or publication was performed.

## Follow-up before rollout

Review the native code and configuration, provision pinned application/venv/model
paths, resolve the parallel proxy-test instability if requiring the exact default
repo gate, and coordinate a separate model-service integration window. Publish or
roll out only through the parent task's approval flow. Retain the existing POC until
that integration is approved.
