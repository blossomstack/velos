import { useState } from "react";
import { Network, X } from "lucide-react";
import { useCreateService, type NewService } from "../api";

export function CreateService({ open, onClose }: { open: boolean; onClose: () => void }) {
  const create = useCreateService();
  const [name, setName] = useState("");
  const [selector, setSelector] = useState("app=demo");
  const [targetPort, setTargetPort] = useState(8080);
  const [nodePort, setNodePort] = useState("");

  if (!open) return null;

  function parseKV(s: string): Record<string, string> {
    const out: Record<string, string> = {};
    for (const part of s.split(/[,\s]+/).filter(Boolean)) {
      const i = part.indexOf("=");
      if (i > 0) out[part.slice(0, i)] = part.slice(i + 1);
    }
    return out;
  }

  const selected = parseKV(selector);

  async function submit() {
    const body: NewService = {
      name: name.trim(),
      selector: selected,
      targetPort,
      // Blank means "let the server pick", which is the common case.
      nodePort: nodePort.trim() ? Number(nodePort) : undefined,
    };
    try {
      await create.mutateAsync(body);
      onClose();
      setName("");
    } catch {
      /* error surfaced below */
    }
  }

  // The server rejects an empty selector rather than reading it as "everything",
  // so refuse it here too instead of sending a request that cannot succeed.
  const valid = name.trim().length > 0 && Object.keys(selected).length > 0 && targetPort > 0;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4">
      <div className="absolute inset-0 bg-black/60" onClick={onClose} />
      <div className="relative w-full max-w-lg rounded-2xl border border-white/10 bg-[#0d0d14] shadow-2xl">
        <div className="flex items-center justify-between border-b border-white/5 px-6 py-4">
          <div className="flex items-center gap-2 text-lg font-semibold text-zinc-100">
            <Network size={18} className="text-indigo-400" />
            Expose a service
          </div>
          <button onClick={onClose} className="rounded-lg p-1.5 text-zinc-400 hover:bg-white/5">
            <X size={18} />
          </button>
        </div>

        <div className="space-y-4 px-6 py-5">
          <Row label="Name">
            <input
              autoFocus
              value={name}
              onChange={(e) => setName(e.target.value.replace(/[^a-z0-9-]/g, ""))}
              placeholder="web"
              className={input}
            />
          </Row>
          <Row label="Selector">
            <input
              value={selector}
              onChange={(e) => setSelector(e.target.value)}
              placeholder="app=web"
              className={`${input} font-mono`}
            />
            <div className="mt-1.5 text-xs text-zinc-600">
              Container labels this service fronts. Every entry must match.
            </div>
          </Row>
          <div className="grid grid-cols-2 gap-4">
            <Row label="Target port">
              <input
                type="number"
                min={1}
                max={65535}
                value={targetPort}
                onChange={(e) => setTargetPort(Math.max(1, +e.target.value))}
                className={input}
              />
              <div className="mt-1.5 text-xs text-zinc-600">Port inside the container.</div>
            </Row>
            <Row label="Node port">
              <input
                type="number"
                min={30000}
                max={32767}
                value={nodePort}
                onChange={(e) => setNodePort(e.target.value)}
                placeholder="auto"
                className={input}
              />
              <div className="mt-1.5 text-xs text-zinc-600">
                Blank assigns one from 30000&ndash;32767.
              </div>
            </Row>
          </div>

          {create.isError && (
            <div className="rounded-lg border border-rose-500/30 bg-rose-500/10 px-3 py-2 text-sm text-rose-300">
              {(create.error as Error).message}
            </div>
          )}
        </div>

        <div className="flex justify-end gap-3 border-t border-white/5 px-6 py-4">
          <button onClick={onClose} className="rounded-lg px-4 py-2 text-sm text-zinc-400 hover:bg-white/5">
            Cancel
          </button>
          <button
            onClick={submit}
            disabled={!valid || create.isPending}
            className="inline-flex items-center gap-2 rounded-lg bg-indigo-500 px-4 py-2 text-sm font-medium text-white shadow-lg shadow-indigo-500/20 hover:bg-indigo-400 disabled:cursor-not-allowed disabled:opacity-40"
          >
            {create.isPending ? "Exposing…" : "Expose"}
          </button>
        </div>
      </div>
    </div>
  );
}

const input =
  "w-full rounded-lg border border-white/10 bg-black/30 px-3 py-2 text-sm text-zinc-100 outline-none focus:border-indigo-400/60 focus:ring-2 focus:ring-indigo-400/20";

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="block">
      <div className="mb-1.5 text-xs font-medium uppercase tracking-wide text-zinc-500">{label}</div>
      {children}
    </label>
  );
}
