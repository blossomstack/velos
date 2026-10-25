# Explicit real-model integration

`adapter.py` is a temporary integration example for the separately provisioned
`audio-provider/v1` Qwen POC, whose release exports `model_service.providers` and
`shared.http`. It is not a bundled provider, installer, or production service.
It uses the existing pinned Python environment and model snapshot without
modifying either. Its process stays in the foreground on loopback, warms the
model with a short synthesis before readiness, reports the Velos launch identity,
and drains admitted inference on SIGTERM. Python request threads are explicitly
non-daemon so `server_close` joins them before process exit.

Only run with operator approval and exclusive use of the model/GPU. First verify
the external snapshot checksums, record existing process ownership and service
configuration, stop the owned existing model, and prepare restoration using its
original controller. Do not signal an arbitrary PID or run two resident models.
The harness does not stop or restore external services for you.

Create a local **NativeConfig** JSON file (without the worker's outer `runtime`
wrapper). Replace every placeholder with an absolute provisioned path:

```json
{
  "stateDirectory": "/absolute/private/new-test-state",
  "profiles": {
    "model": {
      "executable": "/absolute/poc/.venv-mlx/bin/python",
      "args": ["/absolute/velos/scripts/manual-mlx/adapter.py"],
      "workingDirectory": "/absolute/poc",
      "env": {
        "PATH": "/opt/homebrew/bin:/usr/bin:/bin",
        "PYTHONPATH": "/absolute/poc",
        "HF_HOME": "/absolute/poc/var/hf",
        "HF_HUB_OFFLINE": "1",
        "TOKENIZERS_PARALLELISM": "false",
        "PYTHONUNBUFFERED": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
        "MODEL_PORT": "8766"
      },
      "stopTimeoutSeconds": 60,
      "readiness": {"port": 8766, "path": "/health"}
    }
  }
}
```

Build before stopping the existing service. The installed `ffmpeg` must match
the externally pinned POC environment. No download or installation occurs here.

```sh
cargo test -p velos-tests --test manual_mlx --no-run
VELOS_MODEL_CONFIG=/absolute/config.json \
VELOS_MODEL_RESULT=/absolute/result.json \
cargo test -p velos-tests --test manual_mlx -- \
  --ignored --exact real_qwen_api_lifecycle --nocapture
```

The ignored test uses a temporary loopback Velos API server, in-memory store,
worker reconciliation and the real native runtime. It validates initially false
readiness, loaded readiness, real synthesis, an owned-process SIGKILL failure,
OnFailure restart with a new launch identity, hibernation during active synthesis
with successful response and exit code zero, resume, and deletion. Startup waits
are bounded at 180 seconds, HTTP requests at 180 seconds, and native shutdown at
60 seconds plus bounded kill/reap/log cleanup. It never runs in the default CI
suite. Lightweight native fixture tests remain the default gate.

Afterward verify that the temporary adapter exited and its port is free before
restoring the original service. If ownership or exit is uncertain, stop and
investigate; never start a second model to work around an ambiguous outcome.
Verify the restored model by synthesis, compare saved configuration/sample
checksums, and confirm the studio's original PID and launch instance. The
unmodified POC retains its original liveness-only health behavior after restore.

This adapter is deliberately limited: readiness verifies this short warmup, not
all possible inputs or load. The supervisor signals the owned process group;
applications spawning subprocesses must design their own signal/drain behavior.
Here inference is in-process and FFmpeg postprocessing is short-lived. This test
does not establish drain guarantees for arbitrary subprocess trees or long jobs.
