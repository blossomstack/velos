import { useState } from "react";
import { Ban, Server } from "lucide-react";
import { useContainers, useLeases, useWorkers } from "../api";
import { Bar, Card, EmptyState, Labels, Spinner, StatusDot } from "../ui";
import { Drawer, Field, Json } from "../components/Drawer";
import {
  ageFrom,
  boundWorker,
  cpuOf,
  fmtBytes,
  holdsResources,
  isWorkerReady,
  leaseFor,
  memOf,
  phaseOf,
  readyCondition,
  secondsSince,
} from "../format";
import type { Taint, Worker } from "../types";

export function Workers() {
  const { data: workers, isLoading } = useWorkers();
  const { data: leases = [] } = useLeases();
  const { data: containers = [] } = useContainers();
  const [selected, setSelected] = useState<Worker | null>(null);

  if (isLoading) return <Spinner />;
  if (!workers || workers.length === 0)
    return (
      <Card className="p-2">
        <EmptyState
          icon={<Server size={32} />}
          title="No workers registered"
          hint="Start a veloslet pointed at this server with a bootstrap token to register a node."
        />
      </Card>
    );

  return (
    <>
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
        {workers.map((w) => {
          const ready = isWorkerReady(w);
          const lease = leaseFor(leases, w.metadata.name);
          const leaseAge = secondsSince(lease?.spec.renewTime);
          const cordoned = !!w.spec.unschedulable;
          const onNode = containers.filter((c) => boundWorker(c) === w.metadata.name);
          // What the *scheduler* has committed here, which is not the same as
          // what is switched on: a hibernated container's slot is still spoken
          // for. Counting only the live ones advertises room that placement
          // will then refuse to use.
          const committed = onNode.filter(holdsResources);
          const running = onNode.filter((c) => phaseOf(c) === "Running");
          const cpuCap = w.status?.allocatable?.cpu ?? 0;
          const memCap = w.status?.allocatable?.memoryBytes ?? 0;
          const cpuUsed = committed.reduce((a, c) => a + cpuOf(c), 0);
          const memUsed = committed.reduce((a, c) => a + memOf(c), 0);

          return (
            <Card
              key={w.metadata.uid ?? w.metadata.name}
              className="cursor-pointer p-5 transition hover:border-white/10 hover:bg-white/[0.04]"
            >
              <div onClick={() => setSelected(w)}>
                <div className="flex items-start justify-between gap-2">
                  {/* min-w-0 on the flex *item* as well as on the truncating
                      child: without it this block refuses to shrink below its
                      content, and the status column overflows the card's padding
                      instead of the name truncating. */}
                  <div className="flex min-w-0 items-center gap-2.5">
                    <div className="rounded-lg bg-white/5 p-2 text-zinc-400">
                      <Server size={16} />
                    </div>
                    <div className="min-w-0">
                      <div className="truncate font-medium text-zinc-100">{w.metadata.name}</div>
                      <div className="truncate text-[11px] text-zinc-500">
                        {w.status?.nodeInfo?.agentVersion
                          ? `veloslet ${w.status.nodeInfo.agentVersion}`
                          : "unknown agent"}
                        {w.status?.nodeInfo?.os ? ` · ${w.status.nodeInfo.os}` : ""}
                      </div>
                    </div>
                  </div>
                  {/* shrink-0: as the card narrows, the *name* is the thing that
                      can truncate. Letting this column shrink instead pushed the
                      status label — and the cordon chip — off the card edge. */}
                  <div className="flex shrink-0 flex-col items-end gap-1.5">
                    <StatusDot ok={ready} label={ready ? "Ready" : "NotReady"} />
                    {/* A fixed slot, held open whether or not there is a chip to
                        put in it. Letting the chip claim its own height made the
                        one cordoned card in a row taller than its neighbours, so
                        their usage bars stopped lining up — the cards read as a
                        row, and only if their innards agree on a baseline.

                        A cordoned worker is Ready and refusing work: the two are
                        independent, and a green dot alone reads as "available"
                        for a node nothing will be placed on. */}
                    <div className="h-5">
                      {cordoned && (
                        <span className="inline-flex items-center gap-1 rounded-full bg-amber-400/10 px-2 py-0.5 text-[11px] font-medium text-amber-300 ring-1 ring-inset ring-amber-400/30">
                          <Ban size={10} />
                          Cordoned
                        </span>
                      )}
                    </div>
                  </div>
                </div>

                <div className="mt-4 space-y-3">
                  <Usage label="CPU" used={`${cpuUsed}`} total={`${cpuCap}`} u={cpuUsed} t={cpuCap} color="bg-indigo-400" />
                  <Usage
                    label="Memory"
                    used={fmtBytes(memUsed)}
                    total={fmtBytes(memCap)}
                    u={memUsed}
                    t={memCap}
                    color="bg-violet-400"
                  />
                </div>

                <div className="mt-4 flex items-center justify-between border-t border-white/5 pt-3 text-xs text-zinc-500">
                  <span>
                    <span className="font-mono text-zinc-300">{running.length}</span> running
                    {committed.length > running.length && (
                      <span className="text-zinc-600">
                        {" "}
                        · {committed.length - running.length} reserved
                      </span>
                    )}
                  </span>
                  <span>
                    lease{" "}
                    <span className={leaseAge < (lease?.spec.leaseDurationSeconds ?? 40) ? "text-emerald-400" : "text-rose-400"}>
                      {lease ? `${ageFrom(lease.spec.renewTime)} ago` : "none"}
                    </span>
                  </span>
                </div>
              </div>
            </Card>
          );
        })}
      </div>

      <WorkerDrawer worker={selected} leases={leases} onClose={() => setSelected(null)} containers={containers} />
    </>
  );
}

/// Taints, with the effect spelled out. `NoSchedule` is a hard filter and
/// `PreferNoSchedule` only a penalty, so which one it is decides whether an
/// untolerating container can land here at all.
function TaintList({ taints }: { taints: Taint[] }) {
  return (
    <div className="flex flex-wrap gap-1">
      {taints.map((t) => (
        <span
          key={`${t.key}=${t.value ?? ""}:${t.effect}`}
          className={`rounded px-1.5 py-0.5 font-mono text-[11px] ring-1 ring-inset ${
            t.effect === "NoSchedule"
              ? "bg-rose-400/10 text-rose-300 ring-rose-400/30"
              : "bg-amber-400/10 text-amber-300 ring-amber-400/30"
          }`}
          title={t.effect === "NoSchedule" ? "hard filter" : "scoring penalty only"}
        >
          {t.key}
          {t.value ? `=${t.value}` : ""}:{t.effect}
        </span>
      ))}
    </div>
  );
}

function Usage({
  label,
  used,
  total,
  u,
  t,
  color,
}: {
  label: string;
  used: string;
  total: string;
  u: number;
  t: number;
  color: string;
}) {
  return (
    <div>
      <div className="mb-1 flex justify-between text-xs">
        <span className="text-zinc-500">{label}</span>
        <span className="font-mono text-zinc-400">
          {used} <span className="text-zinc-600">/ {total}</span>
        </span>
      </div>
      <Bar used={u} total={t} color={color} />
    </div>
  );
}

function WorkerDrawer({
  worker,
  leases,
  containers,
  onClose,
}: {
  worker: Worker | null;
  leases: import("../types").Lease[];
  containers: import("../types").Container[];
  onClose: () => void;
}) {
  if (!worker) return <Drawer open={false} onClose={onClose} title="" children={null} />;
  const ready = isWorkerReady(worker);
  const condition = readyCondition(worker);
  const lease = leaseFor(leases, worker.metadata.name);
  const info = worker.status?.nodeInfo;
  const onNode = containers.filter((c) => boundWorker(c) === worker.metadata.name);
  const taints = worker.spec.taints ?? [];

  return (
    <Drawer
      open={!!worker}
      onClose={onClose}
      title={worker.metadata.name}
      subtitle={`Worker · uid ${worker.metadata.uid ?? "—"}`}
    >
      <div className="divide-y divide-white/5">
        <Field label="Status">
          <StatusDot ok={ready} label={ready ? "Ready" : "NotReady"} />
        </Field>
        {/* The reason and the transition time are what a NotReady worker owes
            you: the boolean says something is wrong, these say what and for how
            long — and the latter is what the eviction timer runs against. */}
        {condition?.reason && <Field label="Reason">{condition.reason}</Field>}
        {condition?.lastTransitionTime && (
          <Field label={ready ? "Ready since" : "NotReady since"}>
            {ageFrom(condition.lastTransitionTime)} ago
          </Field>
        )}
        <Field label="Agent">{info?.agentVersion ? `veloslet ${info.agentVersion}` : "unknown"}</Field>
        <Field label="OS">{info?.os || "unknown"}</Field>
        <Field label="Arch">{info?.arch || "unknown"}</Field>
        <Field label="Hostname">{info?.hostname || "unknown"}</Field>
        <Field label="Runtime">{worker.status?.containerRuntimeVersion ?? "unknown"}</Field>
        <Field label="Schedulable">{worker.spec.unschedulable ? "No (cordoned)" : "Yes"}</Field>
        <Field label="Taints">
          {taints.length === 0 ? "—" : <TaintList taints={taints} />}
        </Field>
        <Field label="Capacity">
          {worker.status?.capacity?.cpu ?? "—"} cores · {fmtBytes(worker.status?.capacity?.memoryBytes)}
        </Field>
        <Field label="Addresses">
          {worker.status?.addresses?.length ? worker.status.addresses.join(", ") : "—"}
        </Field>
        <Field label="Last seen">{lease ? `${ageFrom(lease.spec.renewTime)} ago` : "never"}</Field>
        <Field label="Lease">
          {lease ? (
            <>
              renewed {ageFrom(lease.spec.renewTime)} ago · {lease.spec.leaseDurationSeconds}s duration
            </>
          ) : (
            "none"
          )}
        </Field>
        <Field label="Created">{ageFrom(worker.metadata.creationTimestamp)} ago</Field>
      </div>

      <div className="mt-6">
        <div className="mb-2 text-sm font-semibold text-zinc-300">
          Containers on this node ({onNode.length})
        </div>
        {onNode.length === 0 ? (
          <div className="text-sm text-zinc-600">None</div>
        ) : (
          <div className="space-y-1.5">
            {onNode.map((c) => (
              <div
                key={c.metadata.name}
                className="flex items-center justify-between rounded-lg bg-white/[0.03] px-3 py-2 text-sm"
              >
                <span className="font-mono text-zinc-300">{c.metadata.name}</span>
                <span className="text-xs text-zinc-500">{phaseOf(c)}</span>
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="mt-6">
        <div className="text-sm font-semibold text-zinc-300">Labels</div>
        <div className="mt-2">
          <Labels labels={worker.metadata.labels} />
        </div>
      </div>

      <div className="mt-6">
        <div className="text-sm font-semibold text-zinc-300">Raw object</div>
        <Json value={worker} />
      </div>
    </Drawer>
  );
}
