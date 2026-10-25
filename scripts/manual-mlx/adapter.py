"""Opt-in integration adapter for the separately provisioned audio POC.

Run with the pinned POC venv and POC_ROOT/PYTHONPATH pointing at its release.
No installs, downloads or changes to the POC. Never run concurrently with another
resident model. This example adds loaded readiness and SIGTERM drain to its API.
"""
import json
import os
import signal
import threading
import time

from model_service.providers import Provider
from shared.http import Handler, LoopbackServer

ready = threading.Event()
draining = threading.Event()
active = threading.Event()
lock = threading.Lock()
instance = os.environ["VELOS_INSTANCE_ID"]
provider = Provider("mlx-qwen3-1.7b")


def record(event, **fields):
    print(json.dumps({"event": event, "pid": os.getpid(), "instance": instance, **fields}), flush=True)


class API(Handler):
    def do_GET(self):
        if not self.allowed():
            return
        if self.path == "/health":
            self.reply({"pid": os.getpid(), "instance": instance,
                        "ready": ready.is_set() and not draining.is_set(),
                        "active": active.is_set(), "draining": draining.is_set()})
        elif self.path == "/v1/capabilities":
            self.reply(provider.capabilities())
        else:
            self.send_error(404)

    def do_POST(self):
        if not self.allowed():
            return
        if self.path != "/v1/synthesize":
            self.send_error(404)
            return
        if draining.is_set() or not ready.is_set():
            self.reply({"error": "Not ready or draining"}, 503)
            return
        if not lock.acquire(blocking=False):
            self.reply({"error": "Provider busy"}, 429)
            return
        try:
            # Recheck after locking so a drain never admits a queued request.
            if draining.is_set():
                self.reply({"error": "Draining"}, 503)
                return
            active.set()
            result = provider.synthesize(self.body())
            self.reply(result)
            record("inference", elapsed_s=result["elapsed_s"], metadata=result["metadata"], sha256=result["sha256"])
        except (ValueError, KeyError) as error:
            self.reply({"error": str(error)}, 400)
        except Exception as error:
            self.reply({"error": str(error)}, 500)
        finally:
            active.clear()
            lock.release()


server = LoopbackServer(("127.0.0.1", int(os.environ.get("MODEL_PORT", "8766"))), API)
# ThreadingHTTPServer normally daemonizes request threads. For draining, keep
# admitted inference alive and join it in server_close before the parent exits.
server.daemon_threads = False


def stop(_signum, _frame):
    if not draining.is_set():
        draining.set()
        ready.clear()
        record("draining", active=active.is_set())
        threading.Thread(target=server.shutdown, daemon=True).start()


signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)


def warm():
    start = time.monotonic()
    try:
        with lock:
            result = provider.synthesize({"language": "en", "voice": "narrator", "text": "Ready.", "seed": 42})
        if not draining.is_set():
            ready.set()
        record("loaded", elapsed_s=round(time.monotonic() - start, 3), metadata=result["metadata"])
    except Exception as error:
        record("load_failed", error=str(error))
        stop(None, None)


warmup = threading.Thread(target=warm)
warmup.start()
record("listening")
try:
    server.serve_forever(poll_interval=0.05)
finally:
    draining.set()
    ready.clear()
    server.server_close()
    warmup.join()
    record("stopped", active=active.is_set())
