# Native macOS workloads (local implementation preview)

MLX needs a native macOS Python process to access Metal. A Linux micro-VM does
not provide that path. Velos can now supervise a foreground native process through
the same assignment, restart, hibernate/resume and deletion reconciler used for
containers. This implementation has not been deployed or validated with a model.

## Provision first, launch second

An operator provisions the release, hash-locked Python environment, and immutable
model snapshot outside Velos. Pin the model repository revision and validate its
file hashes; pin Python packages using a hash-locked dependency file. Velos never
installs Python, invokes a package manager, downloads weights, or deletes model,
release or output directories. Absolute paths identify that approved provisioning.
Use a new release/venv directory for an upgrade, and keep the previous one for rollback.

The existing worker config gains a `runtime` object. Missing `runtime` preserves
`{"type":"AppleContainer"}`. Use the object in [mlx-runtime.json](examples/mlx-runtime.json)
as that field after replacing the illustrative paths. `NativeProcess` needs no
Apple Containerization daemon; `Mixed` accepts the same config and supports both.
Mixed readiness depends on both backends. The state directory's parent must exist;
Velos creates the final directory privately (0700). Use one stable state directory
per worker, never a new directory to evade an ownership/recovery error.

Profiles are local allowlisted commands, not shell strings. The child receives
only the configured environment (no inherited PATH, credentials or shell startup
files), null stdin, explicit argv, and its release working directory. Configure
any required HOME/PATH explicitly. Use absolute executables and foreground servers;
launchers that daemonize, detach sessions or escape the process group are unsupported.
A native workload runs with the worker user's filesystem and device permissions;
this is supervision, not a container sandbox.

Submit [mlx-workload.json](examples/mlx-workload.json) through the existing
Container API/`velosctl apply` workflow. `spec.workload` is a tagged native profile
reference. Existing image workloads omit it. Native admission requires an explicit
`nodeName`, rejects image/command/environment overrides and unknown workload fields,
and makes the runtime/profile reference immutable. Provision the same profile on
the selected worker before submitting. Older workers must not receive native work.
The current UI remains container-oriented; use JSON/API for native definitions.

Keep the model and studio as independent workloads. The model example selects the
POC's `mlx-qwen3-1.7b` provider; the studio can have its own profile with a separate
venv/release and `--provider-url http://127.0.0.1:8766`. Bind both applications only
to loopback. There is no native Service/NodePort forwarding or endpoint publication.
One live instance per profile prevents accidental duplicate launches on the same
profile/port. For the 16 GB Mac, provision **one** model profile, and switch its
release/provider only after draining and stopping it; do not create a second
profile for a second resident model. CPU/memory requests reserve scheduler capacity;
Velos does not enforce native CPU, RAM or Metal limits, or measure model allocation.

## Liveness, readiness, restart and logs

`status.phase=Running` means the owned foreground process is alive. Native
`status.ready` is `null` without a readiness probe, `false` on a failed probe or
exited process, and `true` only for an explicit matching readiness response.
An optional local profile field enables a bounded loopback probe:

```json
"readiness": { "port": 8766, "path": "/ready" }
```

The endpoint must return HTTP success and JSON `{"pid":1234,"ready":true}`, where
`pid` is the actual owned foreground child. A different process's listener cannot
claim readiness. Redirects/proxies are disabled; each request has a one-second
budget and an 8 KiB response cap. Model warm-up and cold-load completion belong to
the model server. The audio POC's liveness-only `/health` is **not** this contract;
the example intentionally leaves readiness unknown until an application readiness
endpoint exists. A failed readiness probe does not restart or kill a model mid-request.

Process exits preserve exit codes (signals are `128 + signal`). Existing restart
policies apply, with native profile launch backoff increasing from 1 to 60 seconds;
a 60-second quiet interval resets it. Explicit hibernate/resume remains available.
Native hibernation is process termination and later fresh execution, not a saved
Metal/RAM checkpoint. Persistent files survive. `SIGTERM` gets the configured
1–60 second grace period, then the owned process group receives `SIGKILL`.
The application must stop accepting and drain requests on TERM, or the operator
must drain it before hibernation/rollout. Velos has no application-specific drain RPC.

Logs are private `<uid>.stdout.log` / `<uid>.stderr.log` files in the state directory.
Each retains the first MiB per launch; further bytes are drained, with truncation
and discarded counts reported through worker tracing. Resume/restart replaces that
UID's logs. Delete retains logs for diagnosis; operators manage retention across
completed UIDs. No remote logs endpoint is added in this implementation.

## Ownership and recovery

Every child is a new process group; signalling is restricted to a live, owned,
unreaped child/group. Exit observation uses `waitid(WNOWAIT)` so the group leader's
PID cannot be reused before group cleanup. No persisted PID, port, command-name
match, `killall` or process search is used to stop anything. Ordinary worker shutdown
stops native children and preserves existing container lifecycle behavior.

An exclusive state-directory file lock prevents concurrent owners. Before spawn,
a durable `<profile>.active` intent record is written. An unclean worker exit may
leave processes running; a subsequent worker refuses to adopt or relaunch while
any such record remains. This deliberately requires operator recovery rather than
risking duplicate resident models or signalling a reused PID. Confirm the old worker
and workload have stopped through trusted OS/operator inspection, retain logs, then
remove only the stale ownership record. Never remove it merely to get startup past
an error. Reboot removes processes but still requires reviewing stale records.

For rollback: drain, hibernate, stop the worker, restore the prior profile's pinned
release/venv paths, then start the worker and resume. No automatic rollout or reboot
service registration is performed here.

## Validation

The native integration suite uses Rust fixture children and `/bin/sleep`, not Python,
MLX, model weights or GPUs. It covers ownership, lifecycle, bounded logs, explicit
environment, exit status, backoff/admission, mixed dispatch, API reconciliation and
readiness identity. Run `cargo test -p velos-tests --test e2e_native` on an execution
host allowed to signal its own process groups and bind ephemeral loopback listeners.
On Mac mini 2 the native suite passes (10 tests plus one intentionally ignored
fixture entrypoint, invoked by the tests). No model services were started or changed.
Darwin returns EPERM for zombie-only groups; cleanup verifies WNOWAIT exit and a
leader-only/empty group before accepting that case, preserving real permission errors.
This behavior is described by Apple's [killpg1 implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c).

Real MLX/Metal execution, application drain behavior and a model-ready endpoint
remain application integration checks. They require a separately approved rollout;
the current audio POC is untouched. The examples contain placeholder provisioning
paths and are not deployment commands.
