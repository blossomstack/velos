# Native workload implementation verification

Date: 2026-10-25. Host: `macmini-2.local`, ARM64 macOS.
Base: `ae1296a0ea962b7c4131bfdd803aa32278e19e56` from `blossomstack/velos`.
Branch: `feat/native-mlx-runtime`. Draft PR published; temporary real-model validation completed below.

## Final gate

```sh
VELOS_TEST_SKIP_APPLE=1 CARGO_NET_OFFLINE=true RUSTC_WRAPPER= make check
```

**Passed with default parallel test execution**, without serial test flags or
name filters. This runs formatting, Clippy with warnings denied, workspace tests
and nine installer fixture cases. The harness reports **247 passed, zero failed,
three ignored**. Of the 247, one is the real Apple test returning through its explicit
safety skip; **246 tests actually execute**. The two ignored functions are child
fixture entrypoints launched explicitly by the native tests. The third is the
opt-in real-model test, executed separately in the approved window below.

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

## Scope and remaining rollout work

The initial implementation used lightweight fixtures only. After separate user
approval, the branch was published as a draft PR and the real-model integration
below was completed. No real Apple container runtime or mini 1 was accessed; no
persistent service registration, machine security change, credential addition,
merge or production deployment occurred.

The restored POC retains its liveness-only health endpoint. Production adoption
would need the model-ready and draining contract in the provisioned application,
plus a separately approved rollout. Example provisioning paths remain placeholders.

## Approved real Qwen integration (2026-10-25)

Draft PR: https://github.com/blossomstack/velos/pull/61. After explicit approval,
ran the ignored `manual_mlx::real_qwen_api_lifecycle` test on mini 2 against the
externally pinned Qwen3-TTS 1.7B CustomVoice 4-bit snapshot
`f35faf19b0cc2160865af64ecf0f22f83d335135`. Verified the snapshot's recorded SHA-256
file manifest first. No environment installation or model download occurred.
The temporary adapter imports the existing POC provider and adds loaded readiness
and SIGTERM draining; it does not alter the POC's server or configuration.
See [manual integration instructions](../scripts/manual-mlx/README.md).

The test exercised the loopback Velos API, temporary in-memory datastore, real
worker reconciliation, and NativeProcess runtime. One model was resident at a
time. The complete test passed in 9.75 seconds:

| Operation | Measured seconds | Verified result |
| --- | ---: | --- |
| Startup through loaded readiness | 3.344 | Initially unready, then owned PID/launch UUID and ready=true |
| Warm synthesis, “Velos is ready.” | 0.650 | 1.36 seconds of generated audio |
| Injected owned-process crash and OnFailure restart | 2.234 | Fresh launch identity and loaded readiness |
| Hibernate during active inference | 0.876 | Request completed successfully; process exited with code 0 |
| Resume through loaded readiness | 2.356 | Fresh launch identity |
| Delete and cleanup | 0.220 | No runtime instance or active ownership record remained |

The drain request generated 1.68 seconds of audio in 0.717 seconds. Per-request
peak MLX allocation was 3,521,932,809 bytes for the first warm inference and
3,725,881,109 bytes for the drain request. These short-input measurements are not
maximum memory requirements or long-request throughput benchmarks. Startup uses
an existing local snapshot and benefits from the host's file cache.

The original POC controller restored Qwen 1.7B after the test; a fresh synthesis
“Restored and ready.” completed in 2.292 seconds and produced a verified SHA-256,
24 kHz mono PCM16 WAV with 2.08 seconds of audio. The restored model PID was
30859, launch instance `287da13411c34d319e40c1fd55623992`. Studio stayed at PID
21201, launch instance `bc5be52d6448489f9350f590e39d64db`. Checksums of POC source,
service/model manifests, and saved runs matched before and after. No temporary
adapter remained. Both services remain on their original loopback addresses.
No persistent supervisor registration or production deployment was performed.

CI's newer Clippy (1.99 versus local 1.96) additionally linted `derive_new`
constructor field initializers. A narrow `redundant_field_names` allowance on the
existing generated model module resolves that compatibility issue without
relaxing handwritten-code linting.
