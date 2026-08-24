import type {
  Container,
  ContainerPhase,
  Lease,
  ObjectMeta,
  Worker,
  WorkerCondition,
} from "./types";

export function fmtBytes(n?: number): string {
  if (!n || n <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v % 1 === 0 ? v : v.toFixed(1)} ${units[i]}`;
}

export function ageFrom(iso?: string, now = Date.now()): string {
  if (!iso) return "—";
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "—";
  let s = Math.max(0, Math.floor((now - then) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  s = s % 60;
  if (m < 60) return `${m}m ${s}s`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

export function secondsSince(iso?: string, now = Date.now()): number {
  if (!iso) return Infinity;
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return Infinity;
  return (now - then) / 1000;
}

// Phase → tailwind class fragments for badges / dots.
export const PHASE_STYLES: Record<ContainerPhase, { text: string; bg: string; dot: string; ring: string }> = {
  Pending: { text: "text-amber-300", bg: "bg-amber-400/10", dot: "bg-amber-400", ring: "ring-amber-400/30" },
  Scheduled: { text: "text-sky-300", bg: "bg-sky-400/10", dot: "bg-sky-400", ring: "ring-sky-400/30" },
  Running: { text: "text-emerald-300", bg: "bg-emerald-400/10", dot: "bg-emerald-400", ring: "ring-emerald-400/30" },
  Hibernated: { text: "text-violet-300", bg: "bg-violet-400/10", dot: "bg-violet-400", ring: "ring-violet-400/30" },
  Succeeded: { text: "text-teal-300", bg: "bg-teal-400/10", dot: "bg-teal-400", ring: "ring-teal-400/30" },
  Failed: { text: "text-rose-300", bg: "bg-rose-400/10", dot: "bg-rose-400", ring: "ring-rose-400/30" },
  Unknown: { text: "text-zinc-400", bg: "bg-zinc-400/10", dot: "bg-zinc-400", ring: "ring-zinc-400/30" },
};

export function phaseOf(c: { status?: { phase?: ContainerPhase } }): ContainerPhase {
  return c.status?.phase ?? "Unknown";
}

// A worker is Ready when it carries a true `Ready` condition.
export function isWorkerReady(w: Worker): boolean {
  return !!w.status?.conditions?.some((c) => c.conditionType === "Ready" && c.status);
}

export function leaseFor(leases: Lease[], workerName: string): Lease | undefined {
  return leases.find((l) => l.metadata.name === workerName || l.spec.holderIdentity === workerName);
}

// ── Server-mirroring rules ────────────────────────────────────────────────
// Everything below encodes a decision the server also makes. Each one names
// the function it must agree with: when these drift, the dashboard reports a
// cluster that does not exist, and nothing fails — it just quietly lies.

/// Which worker a container is actually on.
///
/// The binding lives in `status.workerName` and nowhere else. `spec.nodeName`
/// is the *pin the user asked for* — a hard filter the scheduler applies, not a
/// record of where the container went — and it is null for everything the
/// scheduler placed itself. See `reconcile_scheduling` in
/// `crates/server/src/controllers.rs`.
export function boundWorker(c: Container): string | undefined {
  return c.status?.workerName;
}

/// Phases in which a container still holds a share of its worker's capacity.
///
/// Mirrors `holds_resources` in `crates/server/src/controllers.rs`.
/// `Hibernated` counts: the micro-VM is down but its disk stays on that worker
/// and waking it must not fail because the slot was given away, so the
/// scheduler keeps charging for it. A usage bar that disagrees is telling you
/// there is room the scheduler will refuse to use.
const HOLDS_RESOURCES: ContainerPhase[] = ["Scheduled", "Running", "Hibernated"];

export function holdsResources(c: Container): boolean {
  return HOLDS_RESOURCES.includes(phaseOf(c));
}

// The resources the scheduler charges a container that names none.
// `DEFAULT_CPU` / `DEFAULT_MEM` in `crates/server/src/controllers.rs`.
const DEFAULT_CPU = 1;
const DEFAULT_MEM = 512 * 1024 ** 2;

export function cpuOf(c: Container): number {
  return c.spec.resources?.cpu ?? DEFAULT_CPU;
}

export function memOf(c: Container): number {
  return c.spec.resources?.memoryBytes ?? DEFAULT_MEM;
}

/// Whether the scheduler will place new work on this worker.
///
/// Both halves matter and they fail differently: a NotReady worker has stopped
/// answering, a cordoned one is answering and refusing. `admits_base` in
/// `crates/scheduler/src/lib.rs` rejects either, so neither one's cores are
/// capacity anybody can use.
export function isSchedulable(w: Worker): boolean {
  return isWorkerReady(w) && !w.spec.unschedulable;
}

/// The `Ready` condition itself — for the *reason* and the transition time,
/// which are what a NotReady worker owes you and a boolean cannot carry.
export function readyCondition(w: Worker): WorkerCondition | undefined {
  return w.status?.conditions?.find((c) => c.conditionType === "Ready");
}

/// A deleted object that is waiting on its worker to clean up. The server keeps
/// it listed with a `deletionTimestamp` until the last finalizer clears (see the
/// finalizer protocol in `crates/server/src/lib.rs`), so without this a delete
/// looks like it did nothing at all — the row just carries on saying `Running`.
export function isTerminating(o: { metadata: ObjectMeta }): boolean {
  return !!o.metadata.deletionTimestamp;
}
