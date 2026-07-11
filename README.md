# Almanac

Local-first daily-briefing desktop agent (Rust + Tauri v2). See
`ARCHITECTURE.md` for the design and `BUILD_PHASES.md` for the build plan.

**Status: Phase 0** — scaffold and headless core skeleton. No sources, no
extraction, no synthesis yet.

## Layout

- `crates/almanac-core` — headless core engine (SQLite + migration runner).
  Builds, runs, and is tested without the Tauri shell.
- `src-tauri` — Tauri v2 desktop shell; a client of the core.
- `src` — React + Vite UI (static briefing placeholder).

## Commands

```sh
# headless core self-check (no Tauri shell involved)
cargo run --bin almanac-core -- --self-check   # prints "core ok"

# core tests
cargo test -p almanac-core

# desktop app (dev)
npm install
npm run tauri dev
```

The database is created on first run at `<platform data dir>/Almanac/almanac.db`
(Windows: `%APPDATA%\Almanac\almanac.db`).
