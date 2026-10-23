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
  └── imessage-database         — git dep: dbpecka/imessage-exporter, pinned commit
```

The Rust backend delegates all iMessage parsing and export logic to a fork of [imessage-exporter](https://github.com/ReagentX/imessage-exporter) ([dbpecka/imessage-exporter](https://github.com/dbpecka/imessage-exporter), pinned to an exact commit in `src-tauri/Cargo.toml`), built with two custom feature flags:

- `export` — enables the HTML/TXT/JSON/PDF export runtime
- `write` — enables destructive SQL operations (two-phase delete planning and execution)

### UI flow

The single-page UI walks through five numbered cards in sequence:

1. **Permissions** — checks Full Disk Access (FDA) via `~/Library/Messages/chat.db`. If missing, guides the user to System Settings and relaunches automatically once granted (TCC decisions are cached per-process).

2. **Filters** — date range (accepts `YYYY`, `YYYY-MM`, or `YYYY-MM-DD`) and conversation picker. Chats are listed with resolved contact names and message counts.

3. **Preview** — live preview (400 ms debounce) of how many messages and attachments match the current filters, with breakdown of on-disk vs. not-on-this-Mac (iCloud metadata without local content) vs. missing.

4. **Backup** — format selector (JSON / HTML / PDF / TXT), folder picker (home folder or an external drive), attachments toggle. Each run writes into a new `Bubble Wrap <timestamp>` subfolder; copied attachments are named `<rowid>-<original name>` so same-named files (e.g. every `Audio Message.caf`) never collide. Progress is driven by real-time events from the Rust side. Result shows message count, conversation count, per-attachment copy outcomes (copied / not on this Mac / failed), and the output path. Backups can be encrypted (an AES-256 `.sparsebundle` opened in Finder with a password), and the app checks there's enough free space before starting. PDF exports don't count as a backup for the delete step: the PDF writer can only store Latin-1 text, so emoji, smart quotes and non-Latin scripts become `?`.

5. **Delete** (opt-in, behind a toggle) — shows Messages.app running status and a Messages in iCloud warning (chat.db's own triggers queue every deletion for iCloud sync, so with Messages in iCloud on, deletes are expected to reach your other devices). Scope: delete messages together with their attachments, or attachments only — deleting messages while keeping their attachments isn't offered, because chat.db's triggers delete an attachment's record along with its last message. Requires typed confirmation before executing. Snapshots taken before deletes and cleanups can be restored from the same step (the current database is snapshotted first, so a restore can be undone too).

A **File > Clean Orphaned Data…** menu item opens a modal that scans for and removes two categories of orphaned data: attachment DB rows with no linked message, and files in `~/Library/Messages/Attachments` and `StickerCache` with no DB record. Records and files created in the last 24 hours are skipped, and removing records needs an iCloud acknowledgement unless Messages in iCloud is detected as off (chat.db queues those deletions for iCloud).

### Delete safety contract

`run_delete` enforces these steps in order before touching any data:

1. Typed confirmation phrase must equal `"DELETE"`
2. `backup_verified=true` OR explicit `acknowledge_skip_backup=true`
3. `pgrep -x Messages` must return empty (Messages.app must be closed)
4. If Messages-in-iCloud is detected as **Enabled**, `acknowledge_icloud_sync=true` is required
5. `PRAGMA wal_checkpoint(TRUNCATE)` flushes the WAL on the source DB
6. Atomic snapshot of `chat.db` via SQLite's online backup API written to `~/Library/Application Support/io.pecka.apps.bubblewrap/snapshots/delete/<ISO-timestamp>/` (orphan cleanups use `snapshots/orphan-clean/`; the 5 most recent of each are kept)
7. Delete plan built on a read-only connection, then applied on a separate writable connection
8. Writable connection released immediately after commit
9. Attachment files unlinked after the SQL transaction; failures are logged to stderr with path + reason but are non-fatal

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
- Nothing else: cargo fetches the pinned `imessage-exporter` fork itself. To work on the fork locally, see the `[patch]` note in `src-tauri/Cargo.toml`.

### Run in development

```sh
npm install
npm run tauri dev
```

### Checks

Before pushing, run the same suite CI runs:

```sh
# Rust side
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --lib

# Frontend side
npm run check
```

The suite includes end-to-end tests (`src-tauri/src/e2e_tests.rs`) that run
backup → delete → orphan cleanup against a throwaway home folder whose
`chat.db` uses Apple's real schema and triggers
(`src-tauri/tests/fixtures/chat-db-schema.sql`). They never touch your real
Messages data.

```sh
# just the end-to-end tests
cargo test --manifest-path src-tauri/Cargo.toml --lib e2e
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

dbpecka/imessage-exporter  — the fork (git dependency, pinned)
  imessage-database/
    src/exporters/         — html, txt, json, pdf export engines
    src/tables/write/      — delete.rs (two-phase plan + execute), orphans.rs
```

---

## Restoring from a snapshot

Every delete and orphan cleanup first saves a copy of `chat.db` to
`~/Library/Application Support/io.pecka.apps.bubblewrap/snapshots/<delete|orphan-clean>/<timestamp>/`.
To roll back:

1. Quit Messages (and make sure it stays quit).
2. Replace `~/Library/Messages/chat.db` with the snapshot's `chat.db`.
3. Delete `chat.db-wal` and `chat.db-shm` next to it in `~/Library/Messages/`.
4. Reopen Messages.

Snapshots only contain the database. Attachment files removed by a delete
are not in the snapshot — restore them from your backup's `attachments/`
folder. Backups and snapshots are not encrypted.
