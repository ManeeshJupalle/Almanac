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
};

type SourceStatus = {
  source: string;
  connected: boolean;
  detail: string;
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

  const refresh = useCallback(async () => {
    try {
      const [status, stored] = await Promise.all([
        invoke<SourceStatus[]>("get_connection_status"),
        invoke<Briefing | null>("get_briefing"),
      ]);
      setStatuses(status);
      setBriefing(stored);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

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
      </main>

      <footer className="colophon">
        Every briefed item links back to the exact message or event it came
        from. Raw content never leaves this machine.
      </footer>
    </div>
  );
}

export default App;
