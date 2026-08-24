import { useMemo, useState } from "react";
import { Box, Moon, Play, Plus, Trash2 } from "lucide-react";
import { useContainers, useDeleteContainer, useHibernateContainer, useResumeContainer } from "../api";
import { Card, EmptyState, Labels, PhaseBadge, Spinner } from "../ui";
import { Drawer, Field, Json } from "../components/Drawer";
import { CreateContainer } from "../components/CreateContainer";
import { ageFrom, cpuOf, fmtBytes, isTerminating, memOf, phaseOf } from "../format";
import type { Container, ContainerPhase, NodeAffinity, Toleration } from "../types";

const FILTERS: (ContainerPhase | "All")[] = [
  "All",
  "Running",
  "Pending",
  "Scheduled",
  "Hibernated",
  "Succeeded",
  "Failed",
];

/// Hibernating only makes sense for a container that hasn't finished — the
/// server rejects the rest with a 409, so don't offer the action there.
function canHibernate(phase: ContainerPhase): boolean {
  return phase !== "Succeeded" && phase !== "Failed" && phase !== "Hibernated";
}

export function Containers() {
  const { data: containers, isLoading } = useContainers();
  const del = useDeleteContainer();
  const hibernate = useHibernateContainer();
  const resume = useResumeContainer();
  const [filter, setFilter] = useState<ContainerPhase | "All">("All");
  const [selected, setSelected] = useState<Container | null>(null);
  const [creating, setCreating] = useState(false);

  const rows = useMemo(
    () => (containers ?? []).filter((c) => filter === "All" || phaseOf(c) === filter),
    [containers, filter],
  );

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex gap-1 rounded-lg border border-white/5 bg-white/[0.02] p-1">
          {FILTERS.map((f) => (
            <button
              key={f}
              onClick={() => setFilter(f)}
              className={`rounded-md px-3 py-1.5 text-sm transition ${
                filter === f ? "bg-white/10 text-zinc-100" : "text-zinc-500 hover:text-zinc-300"
              }`}
            >
              {f}
            </button>
          ))}
        </div>
        <button
          onClick={() => setCreating(true)}
          className="inline-flex items-center gap-2 rounded-lg bg-indigo-500 px-4 py-2 text-sm font-medium text-white shadow-lg shadow-indigo-500/20 hover:bg-indigo-400"
        >
          <Plus size={16} />
          Launch container
        </button>
      </div>

      {/* The Node column carries a worker name on every scheduled row, which
          pushes this table past a narrow viewport — and with nothing to scroll,
          the right-hand columns are simply unreachable. */}
      <Card className="overflow-x-auto">
        {isLoading ? (
          <Spinner />
        ) : rows.length === 0 ? (
          <EmptyState
            icon={<Box size={32} />}
            title={filter === "All" ? "No containers yet" : `No ${filter} containers`}
            hint={filter === "All" ? "Launch one to see it scheduled onto a worker." : undefined}
          />
        ) : (
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-white/5 text-left text-xs uppercase tracking-wide text-zinc-500">
                <th className="px-5 py-3 font-medium">Name</th>
                <th className="px-5 py-3 font-medium">Phase</th>
                <th className="px-5 py-3 font-medium">Image</th>
                <th className="px-5 py-3 font-medium">Node</th>
                <th className="px-5 py-3 font-medium">Resources</th>
                <th className="px-5 py-3 font-medium">Age</th>
                <th className="px-5 py-3"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-white/[0.04]">
              {rows.map((c) => (
                <tr
                  key={c.metadata.uid ?? c.metadata.name}
                  onClick={() => setSelected(c)}
                  className="cursor-pointer transition hover:bg-white/[0.03]"
                >
                  <td className="px-5 py-3">
                    <div className="font-medium text-zinc-100">{c.metadata.name}</div>
                    <div className="mt-1">
                      <Labels labels={c.metadata.labels} />
                    </div>
                  </td>
                  <td className="px-5 py-3">
                    <span className="flex flex-wrap items-center gap-1.5">
                      <PhaseBadge phase={phaseOf(c)} />
                      <TerminatingChip container={c} />
                    </span>
                    {/* A container the worker cannot launch keeps its phase —
                        it really is still Scheduled — so the phase alone reads
                        as "any moment now" for something that will never
                        happen. The reason is the answer to "why is this stuck". */}
                    {c.status?.reason && (
                      <div
                        className="mt-1.5 truncate text-xs text-amber-400/90"
                        title={c.status.message ?? c.status.reason}
                      >
                        {c.status.reason}
                      </div>
                    )}
                  </td>
                  <td className="px-5 py-3 font-mono text-xs text-zinc-400">{c.spec.image}</td>
                  <td className="whitespace-nowrap px-5 py-3 text-zinc-400">
                    <NodeCell container={c} />
                  </td>
                  <td className="whitespace-nowrap px-5 py-3 text-zinc-400">
                    {cpuOf(c)} cpu · {fmtBytes(memOf(c))}
                  </td>
                  <td className="whitespace-nowrap px-5 py-3 text-zinc-500">
                    {ageFrom(c.metadata.creationTimestamp)}
                  </td>
                  <td className="px-5 py-3 text-right">
                    {phaseOf(c) === "Hibernated" ? (
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          resume.mutate(c.metadata.name);
                        }}
                        className="rounded-md p-1.5 text-zinc-500 hover:bg-emerald-500/10 hover:text-emerald-400"
                        title="Resume"
                      >
                        <Play size={15} />
                      </button>
                    ) : (
                      canHibernate(phaseOf(c)) && (
                        <button
                          onClick={(e) => {
                            e.stopPropagation();
                            hibernate.mutate(c.metadata.name);
                          }}
                          className="rounded-md p-1.5 text-zinc-500 hover:bg-violet-500/10 hover:text-violet-400"
                          title="Hibernate"
                        >
                          <Moon size={15} />
                        </button>
                      )
                    )}
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        if (confirm(`Delete container "${c.metadata.name}"?`)) del.mutate(c.metadata.name);
                      }}
                      className="rounded-md p-1.5 text-zinc-500 hover:bg-rose-500/10 hover:text-rose-400"
                      title="Delete"
                    >
                      <Trash2 size={15} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Card>

      <ContainerDrawer container={selected} onClose={() => setSelected(null)} />
      <CreateContainer open={creating} onClose={() => setCreating(false)} />
    </div>
  );
}

/// Which worker a container is on.
///
/// The binding lives in `status.workerName` — `spec.nodeName` is the *pin the
/// user asked for*, a request the scheduler weighs and may not honour. Reading
/// only the spec meant every scheduled container read "unscheduled" unless it
/// happened to be pinned, which hid the one thing you need when a container is
/// not running: which worker to go and look at.
function NodeCell({ container }: { container: Container }) {
  const bound = container.status?.workerName;
  if (bound) return <>{bound}</>;
  if (container.spec.nodeName) {
    return (
      <span className="text-zinc-500">
        {container.spec.nodeName} <span className="text-zinc-600">(requested)</span>
      </span>
    );
  }
  return <span className="text-zinc-600">unscheduled</span>;
}

function isTerminal(phase: ContainerPhase): boolean {
  return phase === "Succeeded" || phase === "Failed";
}

/// Whether this phase's status still carries the details of the run — when the
/// container started, and the instance it started as.
///
/// The worker writes status through the status subresource, which replaces the
/// *whole* document, and neither its terminal nor its hibernated status restates
/// those two fields. So they are genuinely gone once a container finishes or goes
/// to sleep — deliberately, since neither still describes anything live.
function keepsRunDetail(phase: ContainerPhase): boolean {
  return !isTerminal(phase) && phase !== "Hibernated";
}

/// An absent field, distinguishing the two reasons it can be absent.
///
/// "Not yet" and "no longer carried" are different answers, and rendering both
/// as an em dash makes a `Succeeded` container look like one whose start time is
/// unknown — when in fact it certainly started, and the phase that replaced its
/// status simply did not restate it.
function Absent({ retained }: { retained: boolean }) {
  if (retained) return <>—</>;
  return (
    <span
      className="text-zinc-600 italic"
      title="the worker replaces the whole status document on each phase change, and this phase does not restate this field"
    >
      not retained
    </span>
  );
}

/// A container the user has deleted that is still listed.
///
/// `DELETE` on a container holding the worker's finalizer marks
/// `metadata.deletionTimestamp` and leaves the object in place until the worker
/// has torn the micro-VM down (the finalizer protocol in
/// `crates/server/src/lib.rs`). Its phase is unchanged and still truthful — it
/// really is `Running` — so with nothing else drawn, a delete looks like a
/// button that did nothing.
function TerminatingChip({ container }: { container: Container }) {
  if (!isTerminating(container)) return null;
  return (
    <span
      className="inline-flex items-center gap-1.5 rounded-full bg-rose-400/10 px-2.5 py-0.5 text-xs font-medium text-rose-300 ring-1 ring-inset ring-rose-400/30"
      title={`deleting since ${container.metadata.deletionTimestamp}`}
    >
      <span className="h-1.5 w-1.5 rounded-full bg-rose-400 live-dot" />
      Terminating
    </span>
  );
}

function TolerationList({ tolerations }: { tolerations: Toleration[] }) {
  return (
    <div className="flex flex-wrap gap-1">
      {tolerations.map((t, i) => (
        <span
          key={`${t.key}-${i}`}
          className="rounded bg-white/[0.04] px-1.5 py-0.5 font-mono text-[11px] text-zinc-400 ring-1 ring-inset ring-white/5"
        >
          {t.key}
          {t.operator === "Exists" ? "" : `=${t.value ?? ""}`}
          {t.effect ? `:${t.effect}` : ""}
        </span>
      ))}
    </div>
  );
}

/// Affinity, counted rather than unrolled. The full term tree is in the raw
/// object below; what the eye needs here is the distinction that changes the
/// outcome — a required term can strand this container forever, a preferred one
/// only nudges the score.
function AffinitySummary({ affinity }: { affinity: NodeAffinity }) {
  const required = affinity.required?.length ?? 0;
  const preferred = affinity.preferred?.length ?? 0;
  if (required === 0 && preferred === 0) return <span className="text-zinc-600">—</span>;
  return (
    <span className="text-xs">
      {required > 0 && (
        <span className="text-amber-300">
          {required} required {required === 1 ? "term" : "terms"}
        </span>
      )}
      {required > 0 && preferred > 0 && <span className="text-zinc-600"> · </span>}
      {preferred > 0 && (
        <span className="text-zinc-400">
          {preferred} preferred {preferred === 1 ? "term" : "terms"}
        </span>
      )}
    </span>
  );
}

function ContainerDrawer({ container, onClose }: { container: Container | null; onClose: () => void }) {
  if (!container) return <Drawer open={false} onClose={onClose} title="" children={null} />;
  const s = container.status ?? {};
  return (
    <Drawer
      open={!!container}
      onClose={onClose}
      title={
        <span className="flex flex-wrap items-center gap-3">
          {container.metadata.name}
          <PhaseBadge phase={phaseOf(container)} />
          <TerminatingChip container={container} />
        </span>
      }
      subtitle={`Container · uid ${container.metadata.uid ?? "—"}`}
    >
      <div className="divide-y divide-white/5">
        <Field label="Image">
          <span className="font-mono text-xs">{container.spec.image}</span>
        </Field>
        <Field label="Command">
          <span className="font-mono text-xs">{container.spec.command?.join(" ") || "—"}</span>
        </Field>
        <Field label="Node">
          <NodeCell container={container} />
        </Field>
        <Field label="Restart policy">{container.spec.restartPolicy ?? "Never"}</Field>
        <Field label="Desired state">{container.spec.desiredState ?? "Running"}</Field>
        <Field label="Resources">
          {cpuOf(container)} cores · {fmtBytes(memOf(container))}
          {!container.spec.resources && <span className="text-zinc-600"> (default)</span>}
        </Field>
        {/* The placement constraints, which are the answer to "why is this
            Pending" whenever the reason is not capacity. They decide real
            scheduler outcomes, so they do not belong only in the raw JSON. */}
        {container.spec.nodeSelector && Object.keys(container.spec.nodeSelector).length > 0 && (
          <Field label="Node selector">
            <Labels labels={container.spec.nodeSelector} />
          </Field>
        )}
        {container.spec.tolerations && container.spec.tolerations.length > 0 && (
          <Field label="Tolerations">
            <TolerationList tolerations={container.spec.tolerations} />
          </Field>
        )}
        {container.spec.affinity && (
          <Field label="Node affinity">
            <AffinitySummary affinity={container.spec.affinity} />
          </Field>
        )}
        <Field label="Container ID">
          <span className="font-mono text-xs">
            {s.containerID ?? <Absent retained={keepsRunDetail(phaseOf(container))} />}
          </span>
        </Field>
        <Field label="Started">
          {s.startedAt ? (
            `${ageFrom(s.startedAt)} ago`
          ) : (
            <Absent retained={keepsRunDetail(phaseOf(container))} />
          )}
        </Field>
        <Field label="Hibernated">
          {s.hibernatedAt ? (
            `${ageFrom(s.hibernatedAt)} ago`
          ) : (
            <Absent retained={!isTerminal(phaseOf(container))} />
          )}
        </Field>
        <Field label="Finished">{s.finishedAt ? `${ageFrom(s.finishedAt)} ago` : "—"}</Field>
        <Field label="Exit code">{s.exitCode ?? "—"}</Field>
        {s.reason && (
          <Field label="Reason">
            <span className="text-amber-400/90">{s.reason}</span>
          </Field>
        )}
        {s.message && (
          <Field label="Message">
            <span className="font-mono text-xs break-words">{s.message}</span>
          </Field>
        )}
        <Field label="Created">{ageFrom(container.metadata.creationTimestamp)} ago</Field>
      </div>

      <div className="mt-6">
        <div className="text-sm font-semibold text-zinc-300">Labels</div>
        <div className="mt-2">
          <Labels labels={container.metadata.labels} />
        </div>
      </div>

      <div className="mt-6">
        <div className="text-sm font-semibold text-zinc-300">Raw object</div>
        <Json value={container} />
      </div>
    </Drawer>
  );
}
