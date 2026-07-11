import { useState, type ReactNode } from "react";
import { Copy, KeyRound, Server, Trash2 } from "lucide-react";
import {
  useCreateJoinToken,
  useCreateToken,
  useJoinTokens,
  useRevokeJoinToken,
  useRevokeToken,
  useTokens,
} from "../api";
import { Card } from "../ui";

/// Token management: worker join (bootstrap) tokens and admin CLI tokens.
export function Tokens() {
  return (
    <div className="space-y-10">
      <JoinTokens />
      <CliTokens />
    </div>
  );
}

const JOIN_TTLS: { label: string; secs: number }[] = [
  { label: "1 hour", secs: 3600 },
  { label: "24 hours", secs: 86400 },
  { label: "7 days", secs: 604800 },
  { label: "30 days", secs: 2592000 },
];

function isExpired(iso?: string): boolean {
  if (!iso) return false;
  const t = new Date(iso).getTime();
  return !Number.isNaN(t) && t < Date.now();
}

// One-time secret reveal, shown once right after minting.
function OnceSecret({ value, hint }: { value: string; hint: ReactNode }) {
  return (
    <div className="mt-4 rounded-lg border border-amber-500/30 bg-amber-500/[0.06] p-4 text-sm">
      <p className="mb-2 font-medium text-amber-300">Copy this now — it will not be shown again.</p>
      <div className="flex items-center gap-2">
        <code className="flex-1 break-all rounded bg-black/40 px-3 py-2 text-xs text-zinc-200">{value}</code>
        <button
          onClick={() => navigator.clipboard?.writeText(value)}
          className="rounded-lg border border-white/10 p-2 text-zinc-300 hover:bg-white/5"
          title="Copy"
        >
          <Copy size={16} />
        </button>
      </div>
      <p className="mt-2 text-zinc-400">{hint}</p>
    </div>
  );
}

function SectionHeader({ title, subtitle }: { title: string; subtitle: string }) {
  return (
    <div>
      <h2 className="text-sm font-semibold text-zinc-200">{title}</h2>
      <p className="mt-0.5 text-xs text-zinc-500">{subtitle}</p>
    </div>
  );
}

// Worker join (bootstrap) tokens: mint (shown once), list, revoke.
function JoinTokens() {
  const { data: tokens = [] } = useJoinTokens();
  const create = useCreateJoinToken();
  const revoke = useRevokeJoinToken();
  const [label, setLabel] = useState("");
  const [ttl, setTtl] = useState(JOIN_TTLS[1].secs);
  const [secret, setSecret] = useState<string | null>(null);

  const onCreate = async () => {
    const r = await create.mutateAsync({ label: label.trim(), ttlSeconds: ttl });
    setSecret(`${r.tokenId}.${r.secret}`);
    setLabel("");
  };

  return (
    <section className="space-y-4">
      <SectionHeader
        title="Join tokens"
        subtitle="A veloslet presents one of these once to register a worker. They auto-expire; revoke to stop further registrations with a leaked token."
      />
      <Card className="p-5">
        <div className="flex items-center gap-3">
          <input
            className="flex-1 rounded-lg border border-white/5 bg-black/30 px-3 py-2 text-sm text-zinc-100 outline-none focus:border-indigo-500/50"
            placeholder="Optional label, e.g. mac-studio or fleet-a"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && onCreate()}
          />
          <select
            className="rounded-lg border border-white/5 bg-black/30 px-3 py-2 text-sm text-zinc-200 outline-none focus:border-indigo-500/50"
            value={ttl}
            onChange={(e) => setTtl(Number(e.target.value))}
          >
            {JOIN_TTLS.map((t) => (
              <option key={t.secs} value={t.secs}>
                {t.label}
              </option>
            ))}
          </select>
          <button
            onClick={onCreate}
            disabled={create.isPending}
            className="inline-flex items-center gap-2 rounded-lg bg-indigo-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-indigo-500 disabled:opacity-50"
          >
            <Server size={16} />
            Create join token
          </button>
        </div>

        {secret && (
          <OnceSecret
            value={secret}
            hint={
              <>
                Register a worker with:{" "}
                <code className="text-zinc-300">veloslet --server {window.location.origin} --token &lt;token&gt;</code>
              </>
            }
          />
        )}
      </Card>

      <Card>
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-white/5 text-left text-xs uppercase tracking-wide text-zinc-500">
              <th className="px-5 py-3 font-medium">Label</th>
              <th className="px-5 py-3 font-medium">Created</th>
              <th className="px-5 py-3 font-medium">Expires</th>
              <th className="px-5 py-3" />
            </tr>
          </thead>
          <tbody>
            {tokens.length === 0 && (
              <tr>
                <td colSpan={4} className="px-5 py-8 text-center text-zinc-600">
                  No join tokens.
                </td>
              </tr>
            )}
            {tokens.map((t) => {
              const expired = isExpired(t.expiresAt);
              return (
                <tr key={t.id} className="border-b border-white/5 last:border-0">
                  <td className="px-5 py-3 text-zinc-200">{t.label || <span className="text-zinc-600">—</span>}</td>
                  <td className="px-5 py-3 text-zinc-500">{t.createdAt || "—"}</td>
                  <td className="px-5 py-3 text-zinc-500">
                    {t.expiresAt || "—"}
                    {expired && (
                      <span className="ml-2 rounded bg-rose-500/10 px-1.5 py-0.5 text-[10px] font-medium text-rose-400">
                        expired
                      </span>
                    )}
                  </td>
                  <td className="px-5 py-3 text-right">
                    <button
                      onClick={() => revoke.mutate(t.id)}
                      className="inline-flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-xs text-rose-400 hover:bg-rose-500/10"
                    >
                      <Trash2 size={14} />
                      Revoke
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </Card>
    </section>
  );
}

// Admin CLI-token management: create a named token (shown once), list, revoke.
function CliTokens() {
  const { data: allTokens = [] } = useTokens();
  // Show only CLI tokens; UI session tokens are an internal detail.
  const tokens = allTokens.filter((t) => t.kind === "cli");
  const create = useCreateToken();
  const revoke = useRevokeToken();
  const [label, setLabel] = useState("");
  const [secret, setSecret] = useState<string | null>(null);

  const onCreate = async () => {
    const l = label.trim();
    if (!l) return;
    const r = await create.mutateAsync(l);
    setSecret(r.token);
    setLabel("");
  };

  return (
    <section className="space-y-4">
      <SectionHeader
        title="CLI tokens"
        subtitle="Long-lived admin tokens for velosctl and API access."
      />
      <Card className="p-5">
        <div className="flex items-center gap-3">
          <input
            className="flex-1 rounded-lg border border-white/5 bg-black/30 px-3 py-2 text-sm text-zinc-100 outline-none focus:border-indigo-500/50"
            placeholder="Token label, e.g. laptop or ci-runner"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && onCreate()}
          />
          <button
            onClick={onCreate}
            disabled={create.isPending || !label.trim()}
            className="inline-flex items-center gap-2 rounded-lg bg-indigo-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-indigo-500 disabled:opacity-50"
          >
            <KeyRound size={16} />
            Create CLI token
          </button>
        </div>

        {secret && (
          <OnceSecret
            value={secret}
            hint={
              <>
                Use it with:{" "}
                <code className="text-zinc-300">velosctl login --token &lt;token&gt; --server {window.location.origin}</code>
              </>
            }
          />
        )}
      </Card>

      <Card>
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-white/5 text-left text-xs uppercase tracking-wide text-zinc-500">
              <th className="px-5 py-3 font-medium">Label</th>
              <th className="px-5 py-3 font-medium">Kind</th>
              <th className="px-5 py-3 font-medium">Expires</th>
              <th className="px-5 py-3" />
            </tr>
          </thead>
          <tbody>
            {tokens.length === 0 && (
              <tr>
                <td colSpan={4} className="px-5 py-8 text-center text-zinc-600">
                  No tokens yet.
                </td>
              </tr>
            )}
            {tokens.map((t) => (
              <tr key={t.id} className="border-b border-white/5 last:border-0">
                <td className="px-5 py-3 text-zinc-200">{t.label}</td>
                <td className="px-5 py-3 text-zinc-400">{t.kind}</td>
                <td className="px-5 py-3 text-zinc-500">{t.expiresAt}</td>
                <td className="px-5 py-3 text-right">
                  <button
                    onClick={() => revoke.mutate(t.id)}
                    className="inline-flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-xs text-rose-400 hover:bg-rose-500/10"
                  >
                    <Trash2 size={14} />
                    Revoke
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </Card>
    </section>
  );
}
