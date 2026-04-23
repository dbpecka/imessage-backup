# Bubble Wrap

A macOS desktop app to export, archive, and prune your iMessages — safely.

Export your message history to HTML, PDF, TXT, or JSON. Optionally delete old messages and attachments from the live Messages database to reclaim disk space.

---

## What it does

- **Export** conversations to HTML (self-contained), PDF, plain text, or NDJSON
- **Back up attachments** alongside exported messages, or skip them
- **Preview deletions** before committing — see exact message and attachment counts
- **Delete** old messages and/or attachments from `chat.db` with a multi-step safety contract
- **Clean orphaned data** — attachment DB rows with no linked message, and files on disk with no DB record
- **Filter by date range and conversation** before any export or delete operation
- **Resolve contact names** directly from AddressBook (no Contacts permission required)

---

## How it works

Bubble Wrap is a [Tauri 2](https://tauri.app) app: a native macOS shell around a SvelteKit web UI. All database access, file I/O, and macOS system calls happen in Rust; the frontend handles UI state and calls the Rust backend via Tauri's IPC bridge.

### Architecture

```
SvelteKit (Svelte 5 + TypeScript)
  └── src/routes/+page.svelte   — single-page app, all UI
  └── src/lib/ipc.ts            — typed wrappers around Tauri invoke()

Tauri IPC bridge

Rust backend (src-tauri/)
  ├── commands/                 — fda, discover, preview, backup, delete, orphans, safety
  ├── core/                     — db_path, filter, contacts, icloud, snapshot, progress
  └── imessage-database         — path dep: ../imessage-exporter (fork)
```

The Rust backend delegates all iMessage parsing and export logic to a local fork of [imessage-exporter](https://github.com/ReagentX/imessage-exporter) (`../imessage-exporter`), built with two custom feature flags:

- `export` — enables the HTML/TXT/JSON/PDF export runtime
- `write` — enables destructive SQL operations (two-phase delete planning and execution)

### UI flow

The single-page UI walks through five numbered cards in sequence:

1. **Permissions** — checks Full Disk Access (FDA) via `~/Library/Messages/chat.db`. If missing, guides the user to System Settings and relaunches automatically once granted (TCC decisions are cached per-process).

2. **Filters** — date range (accepts `YYYY`, `YYYY-MM`, or `YYYY-MM-DD`) and conversation picker. Chats are listed with resolved contact names and message counts.

3. **Preview** — live preview (400 ms debounce) of how many messages and attachments match the current filters, with breakdown of on-disk vs. not-on-this-Mac (iCloud metadata without local content) vs. missing.

4. **Backup** — format selector (JSON / HTML / PDF / TXT), folder picker, attachments toggle. Progress is driven by real-time events from the Rust side. Result shows message count, conversation count, attachments copied, total bytes, and manifest path.

5. **Delete** (opt-in, behind a toggle) — shows Messages.app running status and iCloud sync warning. Scope selector: delete messages + attachments, messages only, or attachments only. Requires typed confirmation before executing.

A **File > Clean Orphaned Data…** menu item opens a modal that scans for and removes two categories of orphaned data: attachment DB rows with no linked message, and files in `~/Library/Messages/Attachments` and `StickerCache` with no DB record.

### Delete safety contract

`run_delete` enforces these steps in order before touching any data:

1. Typed confirmation phrase must equal `"DELETE"`
2. `pgrep -x Messages` must return empty (Messages.app must be closed)
3. `PRAGMA wal_checkpoint(TRUNCATE)` flushes the WAL to the main file
4. File-copy snapshot of `chat.db` + `-wal`/`-shm` sidecars written to `~/Documents/iMessage Backups/snapshots/<ISO-timestamp>/`
5. Delete plan built on a read-only connection, then applied on a separate writable connection
6. Writable connection released immediately after commit
7. Attachment files unlinked after the SQL transaction; failures are counted but non-fatal

### Contact name resolution

Contacts are resolved from `~/Library/Application Support/AddressBook/AddressBook-v22.abcddb` directly via SQLite — no Contacts TCC permission required, only FDA. Phone numbers are normalized for US/CA 10- and 11-digit matching.

### iCloud detection

iCloud Messages sync state is detected via `defaults read com.apple.Messages ICloudSync` (Sonoma/Sequoia) with a fallback to `com.apple.madrid kSyncDisabled` for older macOS releases.

---

## Requirements

- macOS 13 Ventura or later
- Full Disk Access granted to Bubble Wrap in System Settings → Privacy & Security → Full Disk Access
- App sandbox disabled (required for FDA access to `~/Library/Messages`)

---

## Development

### Prerequisites

- Rust (stable)
- Node.js + npm
- The `imessage-exporter` fork checked out at `../imessage-exporter` on the `develop` branch

### Run in development

```sh
npm install
npm run tauri dev
```

### Build

```sh
npm run tauri build
```

The release profile uses LTO, single codegen unit, `opt-level = "s"`, `panic = "abort"`, and symbol stripping.

---

## Project structure

```
src/
  app.css                  — CSS variables (light/dark palettes)
  lib/ipc.ts               — all IPC types and api wrappers
  routes/+page.svelte      — entire application UI

src-tauri/
  src/
    lib.rs                 — Tauri builder, menu, command registration
    error.rs               — AppError (Database, FullDiskAccess, Io, Other)
    commands/
      fda.rs               — check_fda, open_fda_settings, relaunch_app
      discover.rs          — probe_db, list_chats, list_contacts
      preview.rs           — preview_backup
      backup.rs            — run_backup (dispatches to imessage-database exporters)
      delete.rs            — preview_delete, run_delete
      safety.rs            — safety_status
      orphans.rs           — scan_orphans, clean_orphans
    core/
      db_path.rs           — resolves ~/Library/Messages/chat.db
      filter.rs            — FilterSpec → QueryContext translation
      contacts.rs          — AddressBook SQLite integration
      icloud.rs            — iCloud Messages state detection
      messages_app.rs      — pgrep check for Messages.app
      progress.rs          — TauriProgress: ProgressReporter → Tauri events
      snapshot.rs          — WAL checkpoint + timestamped file copy

../imessage-exporter/      — sibling fork (path dependency)
  imessage-database/
    src/exporters/         — html, txt, json, pdf export engines
    src/tables/write/      — delete.rs (two-phase plan + execute), orphans.rs
```
