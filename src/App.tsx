import "./App.css";

function App() {
  const today = new Date().toLocaleDateString(undefined, {
    weekday: "long",
    year: "numeric",
    month: "long",
    day: "numeric",
  });

  return (
    <div className="shell">
      <header className="masthead">
        <span className="wordmark">Almanac</span>
        <span className="today">{today}</span>
      </header>

      <main className="briefing" aria-label="Daily briefing">
        <h1>Today&rsquo;s briefing</h1>
        <section className="empty-state">
          <p className="empty-title">Nothing here yet.</p>
          <p className="empty-hint">
            Once your sources are connected, your day — every item grounded to
            the message or event it came from — will appear here.
          </p>
        </section>
      </main>
    </div>
  );
}

export default App;
