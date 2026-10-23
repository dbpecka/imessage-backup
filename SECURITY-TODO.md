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

## 4. Don't publish the whole `public/` directory

**File:** `public/docker-compose.yml`

nginx mounts `.` (all of `public/`) as the web root, which publishes
`public/.idea/` and `docker-compose.yml` itself.

**Fix:** move site assets into a dedicated subfolder (e.g. `public/site/`)
and mount only that, or add an nginx `location ~ /\. { deny all; }` rule and
remove `public/.idea/` from the repo.

## 5. Repo hygiene

Committed files that shouldn't be: `errors.txt`, `.idea/`, `public/.idea/`,
`BubbleWrap.xcodeproj/project.xcworkspace/xcuserdata/`. Remove them and add
to `.gitignore` so local paths/usernames and dev logs don't leak.

## 6. Backups and snapshots are unencrypted

Every backup (and every `chat.db` snapshot) is a plain copy of the user's
whole message history and attachments. Backups can go to external drives
(often exFAT, with no file permissions) or to folders that sync to iCloud
Drive.

**Done so far:** the backup step says backups aren't encrypted, and warns
more strongly for `/Volumes/…`, iCloud Drive, and Desktop/Documents
destinations. Snapshots live in `~/Library/Application Support/<app>/`
(not synced). The delete panel points out leftover snapshots from earlier
builds in `~/Documents/iMessage Backups/snapshots` (never deleted
automatically).

**Still to do:** offer encrypted backups (e.g. an encrypted APFS sparse
bundle via `hdiutil create -encryption AES-256`, or an encrypted archive),
and consider encrypting snapshots at rest or deleting them after a
configurable period.

## 7. Removed: unused shell plugin

`tauri-plugin-shell` and the `shell:allow-open` capability were registered
but never used by the frontend; both have been removed to shrink the attack
surface. Don't re-add them without a concrete need and a scoped permission.
