# Almanac

Local-first daily-briefing desktop agent (Rust + Tauri v2). See
`ARCHITECTURE.md` for the design and `BUILD_PHASES.md` for the build plan.

**Status: Phase 4** — headless core with source adapters (Gmail, Google
Calendar, Slack), an on-device extraction engine (ONNX all-MiniLM-L6-v2 +
rules), and on-device synthesis (Qwen2.5-0.5B-Instruct GGUF via candle)
producing grounded, validated briefings. No briefing UI yet.

## On-device models (one-time fetch)

Extraction embeds text locally with all-MiniLM-L6-v2; synthesis runs
Qwen2.5-0.5B-Instruct (Q4_K_M GGUF) locally via candle. Model files are not
committed (~580 MB total); fetch once — inference never touches the network:

```sh
mkdir -p models/minilm models/qwen2.5-0.5b-instruct
curl -L -o models/minilm/model.onnx https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx
curl -L -o models/minilm/tokenizer.json https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json
curl -L -o models/qwen2.5-0.5b-instruct/model.gguf https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf
curl -L -o models/qwen2.5-0.5b-instruct/tokenizer.json https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct/resolve/main/tokenizer.json
```

Model-dependent tests skip automatically when the files are absent (e.g. CI).
Synthesis is CPU inference — use `--release` builds for realistic latency.

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
