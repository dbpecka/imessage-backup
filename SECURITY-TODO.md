# Security TODO

Open security / hardening items found during a code review. Data-loss bugs
from the same review (orphan cleanup, delete gating, snapshots) have been
fixed in code; the items below still need work.

## 1. Remove risky hardened-runtime entitlements

**File:** `src-tauri/entitlements.plist`

The app ships with:

- `com.apple.security.cs.disable-library-validation`
- `com.apple.security.cs.allow-unsigned-executable-memory`

Bubble Wrap holds Full Disk Access. With library validation disabled, any
dylib the process can be induced to load (plugin paths, `@rpath` hijacks, a
writable framework directory) runs with the app's TCC grants and can read all
Messages, Contacts, Mail, etc. Unsigned executable memory further weakens
the hardened runtime. Tauri apps do not normally need either (WebKit's JIT
runs in a separate WebContent process).

**Fix:** delete both keys, rebuild, and verify the signed/notarized build
still launches and exports. Only re-add an entitlement if a concrete failure
requires it, and document why.

## 2. Set a Content Security Policy

**File:** `src-tauri/tauri.conf.json` (`app.security.csp: null`)

The webview can invoke destructive commands (`run_delete`, `clean_orphans`)
and renders attacker-influenced strings (contact names, group chat names,
chat identifiers from incoming messages). There is no `{@html}` usage today,
so this is not currently exploitable, but with no CSP a single future XSS
becomes "delete the user's message history".

**Fix:** set a strict CSP, e.g.

```json
"csp": "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: asset:; connect-src ipc: http://ipc.localhost"
```

and confirm the dev server (`devCsp`) still works.

## 3. Pin the `imessage-database` dependency

**Files:** `src-tauri/Cargo.toml`, `.github/workflows/ci.yml`,
`ci_scripts/ci_post_clone.sh`

- `Cargo.toml` uses an unpinned `path` dependency.
- CI and Xcode Cloud clone the fork's `develop` branch HEAD.
- `ci_post_clone.sh` derives the GitHub owner from the origin URL, so a
  build from a fork clones *that* owner's `imessage-exporter`.

Signed release builds can therefore silently pick up whatever is on the
fork's `develop` at build time — a supply-chain risk for an app that holds
Full Disk Access and deletes data.

**Fix:** pin a specific commit (`git = "...", rev = "<sha>"` in
`Cargo.toml`, `ref: <sha>` in the workflow, `git checkout <sha>` in the
Xcode Cloud script) and hard-code the `dbpecka` owner.

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
