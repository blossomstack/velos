import { useState } from "react";
import { Check, Copy, Network, Plus, Trash2 } from "lucide-react";
import { useDeleteService, useServices } from "../api";
import { Card, EmptyState, Labels, Spinner } from "../ui";
import { Drawer, Field, Json } from "../components/Drawer";
import { CreateService } from "../components/CreateService";
import { ageFrom } from "../format";
import type { Service, ServiceEndpoint } from "../types";

/// `address:nodePort` — the unit you paste into a reverse proxy's upstream list.
function upstream(e: ServiceEndpoint): string {
  return `${e.address}:${e.nodePort}`;
}

function endpointsOf(s: Service): ServiceEndpoint[] {
  return s.status?.endpoints ?? [];
}

/// Copy-to-clipboard that only claims success when it succeeded. `writeText`
/// rejects outside a secure context (plain http on a LAN address), and a tick
/// shown there would be a lie about where the text went.
function CopyButton({ text, title, label }: { text: string; title: string; label?: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      onClick={async (e) => {
        e.stopPropagation();
        try {
          await navigator.clipboard.writeText(text);
          setDone(true);
          setTimeout(() => setDone(false), 1200);
        } catch {
          /* no clipboard here — leave the icon alone rather than fake it */
        }
      }}
      className="inline-flex items-center gap-1.5 rounded-md px-1.5 py-1 text-zinc-500 transition hover:bg-white/5 hover:text-zinc-300"
      title={title}
    >
      {done ? <Check size={13} className="text-emerald-400" /> : <Copy size={13} />}
      {label && <span className="text-xs">{done ? "copied" : label}</span>}
    </button>
  );
}

export function Services() {
  const { data: services, isLoading } = useServices();
  const del = useDeleteService();
  const [selected, setSelected] = useState<Service | null>(null);
  const [creating, setCreating] = useState(false);

  const rows = services ?? [];

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-end gap-3">
        <button
          onClick={() => setCreating(true)}
          className="inline-flex items-center gap-2 rounded-lg bg-indigo-500 px-4 py-2 text-sm font-medium text-white shadow-lg shadow-indigo-500/20 hover:bg-indigo-400"
        >
          <Plus size={16} />
          Expose a service
        </button>
      </div>

      <Card>
        {isLoading ? (
          <Spinner />
        ) : rows.length === 0 ? (
          <EmptyState
            icon={<Network size={32} />}
            title="No services yet"
            hint="A service puts a stable port on every worker in front of the containers a label selector picks out."
          />
        ) : (
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-white/5 text-left text-xs uppercase tracking-wide text-zinc-500">
                <th className="px-5 py-3 font-medium">Name</th>
                <th className="px-5 py-3 font-medium">Selector</th>
                <th className="px-5 py-3 font-medium">Ports</th>
                <th className="px-5 py-3 font-medium">Endpoints</th>
                <th className="px-5 py-3 font-medium">Age</th>
                <th className="px-5 py-3"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-white/[0.04]">
              {rows.map((s) => {
                const eps = endpointsOf(s);
                return (
                  <tr
                    key={s.metadata.uid ?? s.metadata.name}
                    onClick={() => setSelected(s)}
                    className="cursor-pointer transition hover:bg-white/[0.03]"
                  >
                    <td className="px-5 py-3 font-medium text-zinc-100">{s.metadata.name}</td>
                    <td className="px-5 py-3">
                      <Labels labels={s.spec.selector} />
                    </td>
                    <td className="px-5 py-3 font-mono text-xs text-zinc-400">
                      {s.spec.ports.map((p) => (
                        <div key={p.nodePort}>
                          :{p.nodePort} <span className="text-zinc-600">&rarr;</span> {p.targetPort}
                        </div>
                      ))}
                    </td>
                    <td className="px-5 py-3">
                      {eps.length === 0 ? (
                        <span className="text-zinc-600">no backends</span>
                      ) : (
                        <span className="inline-flex items-center gap-2">
                          <span className="h-1.5 w-1.5 rounded-full bg-emerald-400 live-dot" />
                          <span className="font-mono text-xs text-zinc-300">{upstream(eps[0])}</span>
                          {eps.length > 1 && (
                            <span className="text-xs text-zinc-500">+{eps.length - 1}</span>
                          )}
                        </span>
                      )}
                    </td>
                    <td className="px-5 py-3 text-zinc-500">{ageFrom(s.metadata.creationTimestamp)}</td>
                    <td className="px-5 py-3 text-right">
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          if (confirm(`Delete service "${s.metadata.name}"?`))
                            del.mutate(s.metadata.name);
                        }}
                        className="rounded-md p-1.5 text-zinc-500 hover:bg-rose-500/10 hover:text-rose-400"
                        title="Delete"
                      >
                        <Trash2 size={15} />
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </Card>

      <ServiceDrawer service={selected} onClose={() => setSelected(null)} />
      <CreateService open={creating} onClose={() => setCreating(false)} />
    </div>
  );
}

function ServiceDrawer({ service, onClose }: { service: Service | null; onClose: () => void }) {
  if (!service) return <Drawer open={false} onClose={onClose} title="" children={null} />;
  const eps = endpointsOf(service);
  return (
    <Drawer
      open={!!service}
      onClose={onClose}
      title={service.metadata.name}
      subtitle={`Service · uid ${service.metadata.uid ?? "—"}`}
    >
      {/* Endpoints first: they are the live answer to "where do I point my
          proxy", and the only part of this object that moves. */}
      <div>
        <div className="flex items-center justify-between">
          <div className="text-sm font-semibold text-zinc-300">Endpoints</div>
          {eps.length > 0 && (
            <CopyButton
              text={eps.map(upstream).join(" ")}
              label="Copy all"
              title="Copy every endpoint as a space-separated upstream list, ready to paste into a reverse proxy"
            />
          )}
        </div>

        {eps.length === 0 ? (
          <div className="mt-2 rounded-lg border border-amber-500/20 bg-amber-500/[0.06] px-3 py-2.5 text-xs text-amber-200/80">
            Nothing is answering. Either no container matches the selector and is{" "}
            <span className="font-medium">Running</span>, or its worker has not advertised an
            address.
          </div>
        ) : (
          <div className="mt-2 space-y-1.5">
            {eps.map((e) => (
              <div
                key={`${e.address}:${e.nodePort}:${e.containerName}`}
                className="flex items-center gap-3 rounded-lg border border-white/5 bg-white/[0.02] px-3 py-2"
              >
                <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-emerald-400 live-dot" />
                <span className="font-mono text-sm text-zinc-100">{upstream(e)}</span>
                <span className="ml-auto truncate text-xs text-zinc-500">
                  {e.containerName} on {e.workerName}
                </span>
                <CopyButton text={upstream(e)} title="Copy this endpoint" />
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="mt-6 divide-y divide-white/5">
        <Field label="Ports">
          <div className="space-y-0.5 font-mono text-xs">
            {service.spec.ports.map((p) => (
              <div key={p.nodePort}>
                {p.name ? `${p.name}: ` : ""}:{p.nodePort} &rarr; {p.targetPort} in container
              </div>
            ))}
          </div>
        </Field>
        <Field label="Selector">
          <Labels labels={service.spec.selector} />
        </Field>
        <Field label="Created">{ageFrom(service.metadata.creationTimestamp)} ago</Field>
      </div>

      <div className="mt-6">
        <div className="text-sm font-semibold text-zinc-300">Raw object</div>
        <Json value={service} />
      </div>
    </Drawer>
  );
}
