# Almanac

Local-first daily-briefing desktop agent (Rust + Tauri v2). See
`ARCHITECTURE.md` for the design and `BUILD_PHASES.md` for the build plan.

**Status: Phase 3** — headless core with source adapters (Gmail, Google
Calendar, Slack) and an on-device extraction engine (ONNX all-MiniLM-L6-v2 +
rules). No synthesis or briefing UI yet.

## On-device model (one-time fetch)

Extraction embeds text locally with all-MiniLM-L6-v2. The model files are not
committed (90 MB); fetch them once — inference itself never touches the network:

```sh
mkdir -p models/minilm
curl -L -o models/minilm/model.onnx https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx
curl -L -o models/minilm/tokenizer.json https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json
```

Embedding-dependent tests skip automatically when the model is absent (e.g. CI).

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
