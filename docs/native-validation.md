# Native workload implementation verification

Date: 2026-10-25. Host: `macmini-2.local`, ARM64 macOS.
Base: `ae1296a0ea962b7c4131bfdd803aa32278e19e56` from `blossomstack/velos`.
Branch: `feat/native-mlx-runtime`. Local implementation only.

## Final gate

```sh
VELOS_TEST_SKIP_APPLE=1 CARGO_NET_OFFLINE=true RUSTC_WRAPPER= make check
```

**Passed with default parallel test execution**, without serial test flags or
name filters. This runs formatting, Clippy with warnings denied, workspace tests
and nine installer fixture cases. The harness reports **247 passed, zero failed,
two ignored**. Of the 247, one is the real Apple test returning through its explicit
safety skip; **246 tests actually execute**. The two ignored functions are child
fixture entrypoints launched explicitly by the native tests.

`VELOS_TEST_SKIP_APPLE=1` prevents the installed Apple Containerization CLI from
starting micro-VMs or interacting with other workloads. Its default test behavior
is unchanged when that variable is absent. The wrapper override avoids the local
sccache sandbox restriction; offline mode uses already available locked crates.
`git diff --check` also passes. No claim is made for actual Apple runtime execution.

## Proxy regression

Earlier parallel runs intermittently lost the listener during Service reassignment.
The old code closed and rebound the same port when its owner changed. Ownership and
backend destinations now change atomically on the existing bound socket; accepted
streams keep their original route. The regression test verifies 50 owner changes,
new connections reaching the correct owner, old streams continuing correctly, and
retention of the original listener task.

The unit fixtures also had a separate bind/release/rebind race in their "free port"
helper, producing connection-refused/reset failures under parallel execution.
They now keep OS-assigned loopback sockets continuously bound from allocation through
cleanup. The production listener constructor is shared with those fixtures.
The full gate passes in parallel; serial execution is no longer needed. The
82-test worker unit suite additionally passed ten consecutive parallel runs
(820 repeated test executions) after this correction.

Initial regression fixtures in the first implementation briefly used wildcard
listeners; final fixtures are loopback-only. Production container NodePort bind
addresses remain unchanged. No persistent listeners or remote routes were configured.

## Native safety review and coverage

Sixteen native integration tests plus a runtime log-drain unit test cover:

- Owned unreaped child/group signalling, unrelated-process isolation, idempotent
  stop/remove, signal exit codes, forced stop, and foreground process trees.
- Exclusive state ownership, actual abrupt owner termination, stale-record refusal,
  symlink rejection, missing executable handling, and bounded launch backoff.
- Literal argv, isolated environment and cwd, bounded logs, continued draining on
  disk-write failure, held output pipes, and cancellation during exit cleanup.
- Distinct launch UUIDs, readiness PID/instance matching, oversized responses,
  HTTP timeouts, and restart during an in-flight probe. No successful stale probe
  can mark a new launch ready.
- Serialized mixed-backend mutations preventing simultaneous claims on one UID;
  real API/scheduler/reconciler hibernate/resume/delete; profile validation and
  environment redaction in config diagnostics.

Output readers use cancellable asynchronous I/O. Exit cleanup has a one-second
budget per pipe; ambiguous cleanup retains its record and rejects relaunch. Unclean
Drop does not block the async executor waiting for a child and retains its recovery
record. Background reaping only uses the owned Child handle, never a saved PID.
Normal shutdown tries all native children even if one fails cleanup.

Darwin returns EPERM for zombie-only process groups. The runtime accepts that case
only after WNOWAIT exit evidence and a successful group query showing no other
member. Real permission errors remain errors. No stop operation adopts or looks up
a process by saved PID, name or listening port.

## Remaining integration work

No real container runtime, MLX/Metal workload, existing audio service or mini 1 was
started, stopped or changed. No persistent service registration, machine security
change, credential addition, push, merge or deployment was performed.

Real-model startup, memory, inference, application drain, readiness adaptation and
rollback still need a separately coordinated integration window. The POC's current
liveness-only health endpoint does not satisfy the optional model-ready contract;
it must report explicit readiness plus its PID and injected `VELOS_INSTANCE_ID`.
Example provisioning paths remain placeholders. Publication and rollout belong to
the parent task's approval flow.
