import { useEffect, useMemo, useState } from "react";

const DEFAULT_FORM = {
  mode: "desktop",
  name: "",
  service: "Wi-Fi",
  open: "https://weather.com/",
  user_data_dir: "",
  host_contains: "",
  url_contains: "",
  methods: "",
};

async function fetchJson(url, options) {
  const response = await fetch(url, options);
  const text = await response.text();
  const data = text ? JSON.parse(text) : null;
  if (!response.ok) {
    throw new Error(data?.error || text || response.statusText);
  }
  return data;
}

function splitCsv(value) {
  return value
    .split(",")
    .map((item) => item.trim())
    .filter(Boolean);
}

function formatStatus(status) {
  if (!status) return "idle";
  return status.replaceAll("_", " ");
}

function formatTimestamp(value) {
  if (!value) return "-";
  return new Date(Number(value)).toLocaleString();
}

function eventLine(event) {
  const status = event.status ? ` [${event.status}]` : "";
  const interaction = event.interaction_kind
    ? ` interaction=${event.interaction_kind}${event.interaction_element ? `:${event.interaction_element}` : ""}`
    : "";
  const req = event.request_summary ? ` req=${event.request_summary}` : "";
  const resp = event.response_summary ? ` resp=${event.response_summary}` : "";
  return `${event.method} ${event.host}${event.path}${status}${interaction}${req}${resp}`;
}

function operationLine(operation) {
  const statuses = Object.entries(operation.statuses || {})
    .map(([status, count]) => `${status}:${count}`)
    .join(" ");
  return `${operation.method.padEnd(6)} ${operation.signature}  count=${operation.request_count}  ${statuses}`;
}

function SessionList({ sessions, currentSessionId, onSelect }) {
  if (!sessions.length) {
    return <div className="empty-block">no recorded sessions</div>;
  }

  return (
    <div className="session-list">
      {sessions.map((session) => (
        <button
          key={session.id}
          type="button"
          className={`session-row${session.id === currentSessionId ? " active" : ""}`}
          onClick={() => onSelect(session.id)}
        >
          <div className="session-title">{session.name}</div>
          <div className="session-meta">
            {session.mode} · {formatStatus(session.status)} · {session.event_count} events
          </div>
          <div className="session-id">{session.id}</div>
        </button>
      ))}
    </div>
  );
}

function App() {
  const [status, setStatus] = useState(null);
  const [detail, setDetail] = useState(null);
  const [currentSessionId, setCurrentSessionId] = useState(null);
  const [form, setForm] = useState(DEFAULT_FORM);
  const [askPrompt, setAskPrompt] = useState(
    "Build me an automation from this workflow and generate the implementation files.",
  );
  const [automation, setAutomation] = useState(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");

  const activeSession = status?.active_session || null;
  const sessions = status?.recent_sessions || [];

  const selectedSession = useMemo(() => {
    if (!currentSessionId) return null;
    return sessions.find((session) => session.id === currentSessionId) || detail?.session || null;
  }, [currentSessionId, detail, sessions]);

  async function refreshStatus() {
    const nextStatus = await fetchJson("/api/status");
    setStatus(nextStatus);
    if (!currentSessionId && nextStatus.recent_sessions?.length) {
      setCurrentSessionId(nextStatus.recent_sessions[0].id);
    }
  }

  async function loadSession(sessionId) {
    setBusy(`loading ${sessionId}`);
    setError("");
    try {
      const nextDetail = await fetchJson(`/api/sessions/${sessionId}`);
      setCurrentSessionId(sessionId);
      setDetail(nextDetail);
      setAutomation(nextDetail.automation || null);
    } catch (loadError) {
      setError(loadError.message);
    } finally {
      setBusy("");
    }
  }

  useEffect(() => {
    refreshStatus().catch((statusError) => setError(statusError.message));
    const timer = setInterval(() => {
      refreshStatus().catch((statusError) => setError(statusError.message));
    }, 2000);
    return () => clearInterval(timer);
  }, []);

  useEffect(() => {
    if (currentSessionId) {
      loadSession(currentSessionId).catch((loadError) => setError(loadError.message));
    }
  }, [currentSessionId]);

  async function beginRecording() {
    setBusy("starting recording");
    setError("");
    try {
      const session = await fetchJson("/api/recordings/begin", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          mode: form.mode,
          name: form.name || null,
          service: form.service,
          open: form.open,
          user_data_dir: form.user_data_dir || null,
          host_contains: splitCsv(form.host_contains),
          url_contains: splitCsv(form.url_contains),
          methods: splitCsv(form.methods),
        }),
      });
      setCurrentSessionId(session.id);
      await refreshStatus();
    } catch (beginError) {
      setError(beginError.message);
    } finally {
      setBusy("");
    }
  }

  async function stopRecording() {
    setBusy("stopping recording");
    setError("");
    try {
      const session = await fetchJson("/api/recordings/stop", { method: "POST" });
      await refreshStatus();
      await loadSession(session.id);
    } catch (stopError) {
      setError(stopError.message);
    } finally {
      setBusy("");
    }
  }

  async function askForAutomation() {
    const sessionId = currentSessionId || "latest";
    setBusy(`asking model for ${sessionId}`);
    setError("");
    try {
      const generated = await fetchJson(`/api/sessions/${sessionId}/ask`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ prompt: askPrompt }),
      });
      setAutomation(generated);
      await loadSession(sessionId);
    } catch (askError) {
      setError(askError.message);
    } finally {
      setBusy("");
    }
  }

  const contextMap = detail?.context_map;
  const normalizedEvents = detail?.normalized_events || [];
  const displayedOperations = contextMap?.operations?.slice(0, 40) || [];
  const displayedEvents = normalizedEvents.slice(0, 120);

  return (
    <div className="screen">
      <header className="header">
        <div>
          <div className="eyebrow">$ workflow studio</div>
          <h1>agent-mcp-b / recorder shell</h1>
        </div>
        <div className="header-status">
          <div>provider={status?.llm_provider || "-"}</div>
          <div>{busy || (activeSession ? `recording:${activeSession.name}` : "idle")}</div>
        </div>
      </header>

      {error ? <div className="error-box">error: {error}</div> : null}

      <div className="layout">
        <aside className="sidebar panel">
          <div className="panel-title">[ recorder ]</div>
          <div className="field-grid">
            <label>
              mode
              <select
                value={form.mode}
                onChange={(event) => setForm((current) => ({ ...current, mode: event.target.value }))}
              >
                <option value="desktop">desktop</option>
                <option value="browser_deep">browser_deep</option>
              </select>
            </label>
            <label>
              name
              <input
                value={form.name}
                onChange={(event) => setForm((current) => ({ ...current, name: event.target.value }))}
                placeholder="discord send-message"
              />
            </label>
            <label>
              service
              <input
                value={form.service}
                onChange={(event) => setForm((current) => ({ ...current, service: event.target.value }))}
              />
            </label>
            <label>
              open
              <input
                value={form.open}
                onChange={(event) => setForm((current) => ({ ...current, open: event.target.value }))}
              />
            </label>
            <label>
              profile
              <input
                value={form.user_data_dir}
                onChange={(event) =>
                  setForm((current) => ({ ...current, user_data_dir: event.target.value }))
                }
                placeholder="/tmp/workflow-profile"
              />
            </label>
            <label>
              host filters
              <input
                value={form.host_contains}
                onChange={(event) =>
                  setForm((current) => ({ ...current, host_contains: event.target.value }))
                }
                placeholder="discord.com,api.example.com"
              />
            </label>
            <label>
              url filters
              <input
                value={form.url_contains}
                onChange={(event) =>
                  setForm((current) => ({ ...current, url_contains: event.target.value }))
                }
                placeholder="/api/,/trpc/"
              />
            </label>
            <label>
              methods
              <input
                value={form.methods}
                onChange={(event) => setForm((current) => ({ ...current, methods: event.target.value }))}
                placeholder="GET,POST"
              />
            </label>
          </div>

          <div className="button-row">
            <button type="button" onClick={beginRecording} disabled={Boolean(activeSession) || Boolean(busy)}>
              begin
            </button>
            <button type="button" onClick={stopRecording} disabled={!activeSession || Boolean(busy)}>
              stop
            </button>
          </div>

          <div className="panel-title top-gap">[ server ]</div>
          <div className="meta-block">
            <div>active={activeSession ? activeSession.id : "-"}</div>
            <div>recent={sessions.length}</div>
            <div>selected={selectedSession?.id || "-"}</div>
          </div>

          <div className="panel-title top-gap">[ sessions ]</div>
          <SessionList
            sessions={sessions}
            currentSessionId={currentSessionId}
            onSelect={setCurrentSessionId}
          />
        </aside>

        <main className="main">
          <section className="panel">
            <div className="panel-title">[ selected session ]</div>
            {selectedSession ? (
              <div className="two-col-meta">
                <div>id={selectedSession.id}</div>
                <div>mode={selectedSession.mode}</div>
                <div>status={formatStatus(selectedSession.status)}</div>
                <div>events={selectedSession.event_count}</div>
                <div>started={formatTimestamp(selectedSession.started_at_ms)}</div>
                <div>stopped={formatTimestamp(selectedSession.stopped_at_ms)}</div>
                <div>recorder={selectedSession.recorder_endpoint || "-"}</div>
                <div>error={selectedSession.error || "-"}</div>
              </div>
            ) : (
              <div className="empty-block">select a session</div>
            )}
          </section>

          <section className="panel">
            <div className="panel-title">[ context summary ]</div>
            {contextMap ? (
              <div className="summary-grid">
                <div>summary={contextMap.summary}</div>
                <div>domains={contextMap.domains.length}</div>
                <div>operations={contextMap.operations.length}</div>
                <div>writes={contextMap.writes.length}</div>
                <div>reads={contextMap.reads.length}</div>
                <div>auth={contextMap.auth_signals.join(", ") || "-"}</div>
              </div>
            ) : (
              <div className="empty-block">no context map yet</div>
            )}
          </section>

          <section className="panel split-panel">
            <div>
              <div className="panel-title">[ operations ]</div>
              <pre className="terminal-block">
                {displayedOperations.length
                  ? displayedOperations.map(operationLine).join("\n")
                  : "no operations"}
              </pre>
            </div>
            <div>
              <div className="panel-title">[ recent events ]</div>
              <pre className="terminal-block">
                {displayedEvents.length ? displayedEvents.map(eventLine).join("\n") : "no events"}
              </pre>
            </div>
          </section>

          <section className="panel split-panel">
            <div>
              <div className="panel-title">[ ask ]</div>
              <textarea
                value={askPrompt}
                onChange={(event) => setAskPrompt(event.target.value)}
                spellCheck={false}
              />
              <div className="button-row top-gap">
                <button type="button" onClick={askForAutomation} disabled={Boolean(busy)}>
                  generate automation
                </button>
              </div>
            </div>
            <div>
              <div className="panel-title">[ automation ]</div>
              <pre className="terminal-block">
                {automation ? JSON.stringify(automation, null, 2) : "no automation generated"}
              </pre>
            </div>
          </section>
        </main>
      </div>
    </div>
  );
}

export default App;
