# Security TODO

Security / hardening items found during code review, with their status.
Data-loss bugs from the same reviews have been fixed in code.

## 1. Done: risky hardened-runtime entitlements removed

`com.apple.security.cs.disable-library-validation` and
`com.apple.security.cs.allow-unsigned-executable-memory` are gone from
`src-tauri/entitlements.plist`. With Full Disk Access granted, library
validation off would have let any dylib the process could be induced to load
run with the app's TCC grants.

**Still to verify on a Mac:** a signed, notarized build launches, exports
and deletes normally. If something fails, re-add only the specific
entitlement it needs and document why here.

## 2. Done: Content Security Policy set

`src-tauri/tauri.conf.json` now sets `app.security.csp` (and `devCsp`, which
also allows the Vite dev server's websocket): scripts only from the app
itself (Tauri adds hashes for SvelteKit's inline bootstrap script), no
plugins, frames, forms or `<base>` tags, and network access limited to
Tauri's IPC. `style-src` allows inline styles because the UI uses `style`
attributes; `dangerousDisableAssetCspModification: ["style-src"]` stops
Tauri adding a nonce there, which would otherwise make browsers ignore
`'unsafe-inline'`.

Checked by serving the production build in headless Chromium with the same
policy: the page renders with no violations, and the same harness does
report a violation when the inline-script hash is left out.

**Still to verify on a Mac:** `npm run tauri dev` and a release build show
no CSP violations in the WebKit inspector.

## 3. Done: `imessage-database` pinned

`src-tauri/Cargo.toml` now depends on the fork by git URL at an exact commit
(`rev = "cffac6a…"`), and `Cargo.lock` records it. CI and the Xcode Cloud
script no longer clone the fork's `develop` (or derive the owner from the
origin URL); cargo fetches the pinned commit. Bump `rev` deliberately after
reviewing the fork's changes. For local work on the fork, use the `[patch]`
override described in `Cargo.toml`.

The pin points at the fork's `develop` (commit `faff56d`: dependency
updates for RUSTSEC advisories in SQLite, quick-xml and lopdf, and
full-resolution images in PDF exports). `cargo audit` and `npm audit` report no
vulnerabilities as of this change; the remaining `cargo audit` warnings
are unmaintained or unsound crates deep in Tauri's dependency tree with no
fix available.

## 4. Done: website serves only the site

The site's files moved to `public/site/`, and `public/docker-compose.yml`
mounts only that folder as nginx's web root, so the compose file and any
editor folders next to it are no longer published.

**If the site is deployed some other way** (copying `public/` somewhere),
point that at `public/site/` instead.

## 5. Done: repo hygiene

`errors.txt`, `.idea/`, `public/.idea/` and Xcode's per-user
`xcuserdata/` are removed and listed in `.gitignore`. They remain in the
git history; rewriting history to purge them would break existing clones,
and they held only local paths, a username and a dev log.

## 6. Backups: encryption available; snapshots still unencrypted

Backups and `chat.db` snapshots are full copies of the user's message
history.

**Done:** backups can be encrypted — an AES-256 encrypted APFS sparse bundle
created with `hdiutil` (password passed on stdin, never as an argument;
redacted from `Debug` output), mounted only while the backup is written and
unmounted on every exit path. Unencrypted backups say so, with a stronger
warning for `/Volumes/…`, iCloud Drive, and Desktop/Documents destinations.
Snapshots live in `~/Library/Application Support/<app>/` (not synced), five
kept per kind.

**Done:** snapshots expire. They're removed after 30 days by default
(1 week, 90 days, or "until replaced" in the Restore section), at launch,
after each delete, cleanup or restore, and when the setting changes. Any
snapshot can also be deleted from that list.

**Still to do:** snapshots are not encrypted, so they can be restored
without a password prompt; encrypting them at rest would need one.
Verify the real `hdiutil` flow on a Mac (`docs/MAC-TEST-CHECKLIST.md`,
section 4) — tests use a stand-in.

## 7. Removed: unused shell plugin

`tauri-plugin-shell` and the `shell:allow-open` capability were registered
but never used by the frontend; both have been removed to shrink the attack
surface. Don't re-add them without a concrete need and a scoped permission.
