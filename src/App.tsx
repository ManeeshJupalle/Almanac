import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import "./App.css";

type BriefingItem = {
  position: number;
  kind: "commitment" | "event" | "action_needed" | "noise";
  summary: string;
  occurredAt: string;
  source: "gmail" | "gcal" | "slack";
  nativeId: string;
  deepLink: string;
};

type Briefing = {
  briefingDate: string;
  backendId: string;
  rationale: string;
  createdAt: string;
  items: BriefingItem[];
  preview: BriefingItem[];
  previewDate: string;
};

type SourceStatus = {
  source: string;
  connected: boolean;
  detail: string;
};

type Evidence = {
  tier: "hard" | "soft";
  kind: string;
  source: string;
  nativeId: string;
  deepLink: string;
  observedAt: string;
};

type Proposal = {
  id: number;
  kind: "gmail_reply" | "slack_post" | "jira_transition" | "jira_comment";
  state:
    | "proposed"
    | "approved"
    | "rejected"
    | "expired"
    | "executed"
    | "execution_failed";
  subject: string | null;
  body: string;
  assertsWorkDone: boolean;
  backendId: string;
  createdAt: string;
  expiresAt: string;
  dryRun: string;
  receipt: string | null;
  correlationRationale: string | null;
  evidence: Evidence[];
};

const PROPOSAL_KIND_LABEL: Record<Proposal["kind"], string> = {
  gmail_reply: "Gmail reply",
  slack_post: "Slack post",
  jira_transition: "Jira transition",
  jira_comment: "Jira comment",
};

const KIND_LABEL: Record<BriefingItem["kind"], string> = {
  event: "Event",
  action_needed: "Action",
  commitment: "Commitment",
  noise: "Noise",
};

const SOURCE_LABEL: Record<BriefingItem["source"], string> = {
  gmail: "Gmail",
  gcal: "Calendar",
  slack: "Slack",
};

function timeOf(iso: string): string {
  return new Date(iso).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function App() {
  const [statuses, setStatuses] = useState<SourceStatus[]>([]);
  const [briefing, setBriefing] = useState<Briefing | null>(null);
  const [loading, setLoading] = useState(true);
  const [composing, setComposing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [proposals, setProposals] = useState<Proposal[]>([]);
  const [busyId, setBusyId] = useState<number | null>(null);
  const [chainStatus, setChainStatus] = useState<string | null>(null);

  const loadProposals = useCallback(async () => {
    try {
      const [queue, chain] = await Promise.all([
        invoke<Proposal[]>("list_proposals"),
        invoke<string>("verify_audit_chain"),
      ]);
      setProposals(queue);
      setChainStatus(chain);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refresh = useCallback(async () => {
    try {
      const [status, stored] = await Promise.all([
        invoke<SourceStatus[]>("get_connection_status"),
        invoke<Briefing | null>("get_briefing"),
      ]);
      setStatuses(status);
      setBriefing(stored);
      await loadProposals();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [loadProposals]);

  const decide = useCallback(
    async (id: number, action: "approve" | "reject" | "execute") => {
      setBusyId(id);
      setError(null);
      try {
        const cmd =
          action === "approve"
            ? "approve_proposal"
            : action === "reject"
              ? "reject_proposal"
              : "execute_proposal";
        await invoke<string>(cmd, { id });
        await loadProposals();
      } catch (e) {
        setError(String(e));
        await loadProposals();
      } finally {
        setBusyId(null);
      }
    },
    [loadProposals],
  );

  useEffect(() => {
    refresh();
  }, [refresh]);

  const compose = useCallback(async () => {
    setComposing(true);
    setError(null);
    try {
      const fresh = await invoke<Briefing>("run_live_briefing");
      setBriefing(fresh);
    } catch (e) {
      setError(String(e));
    } finally {
      setComposing(false);
    }
  }, []);

  const openSource = useCallback(async (item: BriefingItem) => {
    try {
      // Defense in depth (audit F-4): the opener capability is already scoped
      // to the source hosts, but re-check here so a bad deep link is refused
      // in the renderer before it ever reaches the OS.
      const url = new URL(item.deepLink);
      const hostAllowed =
        url.protocol === "https:" &&
        (url.hostname === "mail.google.com" ||
          url.hostname === "www.google.com" ||
          url.hostname === "calendar.google.com" ||
          url.hostname.endsWith(".slack.com"));
      if (!hostAllowed) {
        setError(`Refused to open an unexpected link: ${item.deepLink}`);
        return;
      }
      await openUrl(item.deepLink);
    } catch (e) {
      setError(`Could not open source: ${String(e)}`);
    }
  }, []);

  const today = new Date().toLocaleDateString(undefined, {
    weekday: "long",
    month: "long",
    day: "numeric",
    year: "numeric",
  });

  return (
    <div className="shell">
      <header className="masthead">
        <div className="masthead-left">
          <span className="wordmark">Almanac</span>
          <span className="edition">local-first · grounded</span>
        </div>
        <span className="today">{today}</span>
        <div className="sources" role="list" aria-label="Connected sources">
          {statuses.map((s) => (
            <span
              key={s.source}
              role="listitem"
              className={`source-chip ${s.connected ? "on" : "off"}`}
              title={s.detail}
            >
              <i className="dot" aria-hidden />
              {SOURCE_LABEL[s.source as BriefingItem["source"]] ?? s.source}
            </span>
          ))}
        </div>
      </header>

      <main className="page">
        {error && (
          <div className="banner error" role="alert">
            <span>{error}</span>
            <button onClick={() => setError(null)} aria-label="Dismiss error">
              ×
            </button>
          </div>
        )}

        {loading ? (
          <section className="state">
            <p className="state-title">Opening the almanac…</p>
          </section>
        ) : briefing ? (
          <>
            <section className="briefing-head">
              <p className="kicker">Daily briefing</p>
              <h1 className="briefing-date">
                {new Date(briefing.briefingDate + "T00:00:00").toLocaleDateString(undefined, {
                  weekday: "long",
                  month: "long",
                  day: "numeric",
                })}
              </h1>
              <p className="provenance-line">
                composed {briefing.createdAt} UTC · {briefing.backendId} · on-device
              </p>
            </section>

            <section className="rationale" aria-label="Why this order">
              <span className="rationale-mark" aria-hidden>
                ¶
              </span>
              <p>{briefing.rationale}</p>
            </section>

            {briefing.items.length === 0 ? (
              <section className="state">
                <p className="state-title">A clear day.</p>
                <p className="state-hint">
                  Nothing needed your attention — noise was filed away, and the
                  ledger stays open.
                </p>
              </section>
            ) : (
              <ol className="ledger">
                {briefing.items.map((item, i) => (
                  <li key={`${item.source}:${item.nativeId}`} style={{ animationDelay: `${i * 70}ms` }}>
                    <button
                      className="entry"
                      onClick={() => openSource(item)}
                      title={`Open in ${SOURCE_LABEL[item.source]} — ${item.deepLink}`}
                    >
                      <span className="index">{String(i + 1).padStart(2, "0")}</span>
                      <span className="entry-body">
                        <span className="entry-top">
                          <span className={`kind kind-${item.kind}`}>
                            {KIND_LABEL[item.kind]}
                          </span>
                          <span className="when">{timeOf(item.occurredAt)}</span>
                        </span>
                        <span className="summary">{item.summary}</span>
                        <span className="entry-meta">
                          <span className="chip">{SOURCE_LABEL[item.source]}</span>
                          <span className="open-hint">open source ↗</span>
                        </span>
                      </span>
                    </button>
                  </li>
                ))}
              </ol>
            )}

            {briefing.preview.length > 0 && (
              <section className="preview" aria-label="Coming up tomorrow">
                <h2 className="preview-head">
                  Coming up
                  <span className="preview-date">
                    {new Date(briefing.previewDate + "T00:00:00").toLocaleDateString(undefined, {
                      weekday: "long",
                      month: "long",
                      day: "numeric",
                    })}
                  </span>
                </h2>
                <ul className="preview-list">
                  {briefing.preview.map((item) => (
                    <li key={`${item.source}:${item.nativeId}`}>
                      <button
                        className="preview-entry"
                        onClick={() => openSource(item)}
                        title={`Open in ${SOURCE_LABEL[item.source]} — ${item.deepLink}`}
                      >
                        <span className="preview-when">{timeOf(item.occurredAt)}</span>
                        <span className="preview-summary">{item.summary}</span>
                        <span className="chip">{SOURCE_LABEL[item.source]}</span>
                      </button>
                    </li>
                  ))}
                </ul>
              </section>
            )}
          </>
        ) : (
          <section className="state roomy">
            <p className="kicker">Daily briefing</p>
            <p className="state-title">No briefing on record.</p>
            <p className="state-hint">
              Compose one from your connected sources — fetched, classified,
              and sequenced entirely on this machine.
            </p>
            <button className="compose" onClick={compose} disabled={composing || loading}>
              {composing ? "Composing…" : "Compose today's briefing"}
            </button>
            {composing && (
              <p className="composing-note">
                fetching sources → extracting on-device → sequencing with the
                local model — about a minute.
              </p>
            )}
          </section>
        )}

        {briefing && !loading && (
          <div className="actions">
            <button className="compose" onClick={compose} disabled={composing}>
              {composing ? "Composing…" : "Compose fresh briefing"}
            </button>
            {composing && (
              <p className="composing-note">
                fetching sources → extracting on-device → sequencing with the
                local model — about a minute.
              </p>
            )}
          </div>
        )}

        {!loading && proposals.length > 0 && (
          <section className="queue" aria-label="Approval queue">
            <h2 className="queue-head">
              Approval queue
              <span className="queue-sub">
                nothing is sent until you approve it — you approve the exact
                bytes shown
              </span>
            </h2>
            <ul className="proposal-list">
              {proposals.map((p) => (
                <li key={p.id} className={`proposal state-${p.state}`}>
                  <div className="proposal-top">
                    <span className="proposal-kind">
                      {PROPOSAL_KIND_LABEL[p.kind]}
                    </span>
                    <span className={`proposal-state chip state-chip-${p.state}`}>
                      {p.state.replace("_", " ")}
                    </span>
                    {p.assertsWorkDone && (
                      <span
                        className="proposal-claim"
                        title="This draft asserts work was done — it required hard evidence (E1)."
                      >
                        factual claim
                      </span>
                    )}
                  </div>

                  {p.correlationRationale && (
                    <p
                      className="correlation-rationale"
                      title="Why Almanac proposed this — the deterministic correlation basis and its confidence."
                    >
                      {p.correlationRationale}
                    </p>
                  )}

                  <div className="evidence" aria-label="Evidence chain">
                    {p.evidence.map((e, i) => (
                      <button
                        key={`${p.id}:${i}`}
                        className={`evidence-ref tier-${e.tier}`}
                        onClick={() =>
                          openSource({
                            deepLink: e.deepLink,
                          } as unknown as BriefingItem)
                        }
                        title={`${e.tier} evidence — ${e.source}:${e.nativeId} — ${e.deepLink}`}
                      >
                        <span className="tier-badge">{e.tier}</span>
                        <span className="evidence-kind">{e.kind}</span>
                        <span className="open-hint">↗</span>
                      </button>
                    ))}
                  </div>

                  <p className="dry-run-label">
                    Exactly what will be sent (dry run):
                  </p>
                  <pre className="dry-run">{p.dryRun}</pre>

                  {p.receipt && (
                    <pre className="receipt">receipt: {p.receipt}</pre>
                  )}

                  <div className="proposal-actions">
                    {p.state === "proposed" && (
                      <>
                        <button
                          className="approve"
                          disabled={busyId === p.id}
                          onClick={() => decide(p.id, "approve")}
                        >
                          Approve
                        </button>
                        <button
                          className="reject"
                          disabled={busyId === p.id}
                          onClick={() => decide(p.id, "reject")}
                        >
                          Reject
                        </button>
                      </>
                    )}
                    {p.state === "approved" && (
                      <button
                        className="execute"
                        disabled={busyId === p.id}
                        onClick={() => decide(p.id, "execute")}
                      >
                        {busyId === p.id ? "Sending…" : "Send now"}
                      </button>
                    )}
                  </div>
                </li>
              ))}
            </ul>
          </section>
        )}
      </main>

      <footer className="colophon">
        Every briefed item links back to the exact message or event it came
        from. Raw content never leaves this machine.
        {chainStatus && <span className="chain-status"> · {chainStatus}</span>}
      </footer>
    </div>
  );
}

export default App;
