# Downloading Purged iCloud Attachments — Options Matrix

With "Messages in iCloud" enabled, macOS purges attachment files from
`~/Library/Messages/Attachments/…` once they've synced to CloudKit, while
keeping the `attachment` row in `chat.db` intact. The working set on this
machine (2026-04-22):

| bucket                                    | count  |
|-------------------------------------------|-------:|
| purged / CloudKit-available (`transfer_state=0 AND ck_sync_state=1`) | 19,235 |
| purged, alt sync state (`transfer_state=0 AND ck_sync_state=2`)      | 13     |
| locally present (`transfer_state=5`)                                 | 3,014  |
| unknown / error (`transfer_state=-1`)                                | 41     |

We want a programmatic way to re-download the purged set. Below are every
approach investigated, with their verdicts.

---

## The Real API (what Messages.app itself uses)

Private — `IMCore.framework`, confirmed via `dyld_info -exports` + ObjC
runtime introspection:

```objc
// Per-chat bulk trigger (one call handles an entire conversation):
IMChat *chat = [[IMChatRegistry sharedRegistry] _chat_forGUID:guid];
[chat downloadPurgedAttachments];

// Per-GUID batched download:
[[IMFileTransferCenter sharedInstance]
    retrieveLocalFileURLForFileTransferWithGUIDs:@[guid1, guid2, ...]
                                         options:0
                                      completion:^(id result) { ... }];

// Predicate exactly matching the purged-but-downloadable state:
[transfer _isMissingAndDownloadable];

// Completion signal:
IMChatPurgedAttachmentsDownloadCompleteNotification      // chat done
IMChatPurgedAttachmentsDownloadBatchCompleteNotification // batch done
IMFileTransferFinishedNotification                       // per-transfer
```

There's also a Swift `IMCore.ImportExport.AttachmentDownloader` async iterator
that wraps this — it's what Apple's own Messages export flow uses.

**Why we can't call it directly.** Experimentally verified:
`[[IMDaemonController sharedInstance] connectToDaemonWithLaunch:YES ...]`
returns `YES`, but `isConnected` never flips to `YES` — the daemon silently
refuses to serve a client that lacks Apple-restricted entitlements:

- `com.apple.imagent.chat`
- `com.apple.private.imcore.imdpersistence.database-access`
- `com.apple.private.security.storage.Messages`

These are `com.apple.private.*` / Apple-restricted — third-party developers
cannot legitimately code-sign a binary claiming them on a stock macOS install.

---

## Option 1 — AX-driven "Download all attachments" (**abandoned on macOS 26**)

Original plan: drive Messages.app through the Accessibility API to click its
own "Download all attachments" button in the chat info panel. Messages would
then fire the real `IMFileTransferCenter` call with its real entitlements,
and we'd poll `chat.db.transfer_state` for completion.

**Why abandoned.** Apple removed the button from Messages.app somewhere
between Ventura and macOS 26. Verified on macOS 26.3 by live AX introspection:

- **Conversation → Show Details** opens a details panel with a "Photos"
  section header (`AXGroup desc=Photos`) and a thumbnail grid.
- There is no "Download all attachments" button anywhere in the panel.
- No "See All" / "Download original" button on the Photos header either.
- Clicking the Photos header / group is a no-op — no gallery view opens.
- Enumerated every `AXButton` descendant of the window post-Show-Details:
  only Call / FaceTime / screen-sharing / location / contact-info actions.

Messages on macOS 26 is also SwiftUI-based: no `AXRow`/`AXOutline`/`AXTable`
hierarchy, so even the preliminary step of selecting a sidebar row via
familiar AX queries requires workarounds (the conversation list *is*
reachable via `AXButton desc="Contact photo for …"` children of `AXGroup`
rows, and the sidebar search field is reachable as `AXTextField desc=Search`
— those still work — but there's no button to click at the end of the
flow).

For older macOS / Messages versions where the button still exists, this
approach is viable. For macOS 26+, it isn't.

**Current app behaviour.** We surface a clear "only locally-stored
attachments will be backed up" notice in the Preview step whenever
`notOnMacCount > 0`, and point users to either manually scroll Messages
(lazy-loads as it renders) or the SIP/AMFI-off power-user path below.

---

## Option 2 — SIP + AMFI disabled, self-signed with Apple-private entitlements

The Beeper / [Barcelona](https://github.com/beeper/barcelona) recipe. With
SIP disabled and AMFI permissive, you can self-sign a helper binary with
`com.apple.imagent.chat` + `com.apple.private.imcore.imdpersistence.database-access`
and actually connect to `imagent`. From there the real `[chat downloadPurgedAttachments]`
and `IMFileTransferCenter.retrieveLocalFileURLForFileTransferWithGUIDs:…` APIs work.

Required user steps:

1. `sudo nvram boot-args='amfi_get_out_of_my_way=1'`
2. Apple Silicon: Recovery → **Startup Security** → Permissive Security.
   Intel: `csrutil disable` from Recovery.
3. Drop `com.apple.security.xpc.plist` (setting `RestrictRoleAccountServices=false`)
   in `/Library/Preferences/`.
4. Sign the helper with the full set of `com.apple.private.imcore.*` entitlements.

**Pros**
- Clean programmatic API — batch, async, no UI involvement
- Fast: limited by CloudKit bandwidth, not UI rendering
- Direct `_isMissingAndDownloadable` inspection + notification-based completion

**Cons**
- Materially weakens macOS security; not acceptable as a default
- Must be re-applied after OS updates
- Redistributing a binary with Apple-private entitlements is a legal/policy gray area
- Users who don't want to do this are excluded

Ship as opt-in "power user" mode if at all.

---

## Option 3 — `.bundle` injection into Messages.app (BlueBubbles approach)

Package a `.bundle` that gets loaded into `com.apple.MobileSMS` (Messages.app)
itself, so our code inherits Messages' entitlements by virtue of running
in-process. Inside the bundle:

```objc
// At +load, gated on bundleIdentifier == com.apple.MobileSMS:
IMChat *chat = [[IMChatRegistry sharedRegistry] _chat_forGUID:guid];
[chat downloadPurgedAttachments];
```

Reference: [BlueBubbles helper source](https://github.com/BlueBubblesApp/bluebubbles-helper)
(`BlueBubblesHelper.m`, `IMFileTransferCenter.h`, `IMChat.h`, `IMChatRegistry.h`).

Load mechanism: MacForge / mySIMBL / equivalent — a system-wide injection
loader that the user installs once.

**Pros**
- Same real API as Option 2, without SIP/AMFI changes
- Single call per chat covers everything

**Cons**
- Requires MacForge (or similar) pre-installed — itself a kernel-adjacent loader
- Progressively harder to make work on newer macOS (library-validation, hardened runtime)
- Messages.app must be running (and the bundle auto-loaded) at download time
- Unappealing distribution story for a one-off utility app

---

## Dead ends (investigated, not viable)

- **Direct CloudKit to `com.apple.messages.cloud` / service `Messages3`.**
  System-private container; Apple does not provision third-party CloudKit
  entitlements for it. Keys live in the `ichat` keychain-access-group,
  which is also restricted. No public prior art of anyone cracking this.
- **AppleScript `tell application "Messages"`.** The scripting dictionary
  (`Messages.sdef`) only exposes `send`, `login`, `logout` commands and
  read-only `file transfer` properties. No download verb.
- **Mutating `chat.db` `transfer_state` + `ck_sync_state` directly.** Imagent
  owns the DB while running; writes race with it and can corrupt. Even if
  the write sticks, imagent does not re-scan and pull based on row changes.
- **Toggling "Messages in iCloud" off/on.** Works but all-or-nothing, takes
  hours, and cannot be automated programmatically.
- **`brctl` / `bird` / FileProvider.** Messages attachments don't live in
  iCloud Drive — they're in a separate CloudKit container.
- **`cktool`.** Dev-only CloudKit CLI; no access to system-private containers.
- **Spoofing listener ID to `com.apple.MobileSMS`.** Tested — daemon still
  refuses, it's doing entitlement / code-requirement checks, not just an
  ID allow-list.

---

## Current shipping behaviour

- No automatic download trigger. The Preview step shows a prominent
  "Only locally-stored attachments will be backed up" notice whenever
  `notOnMacCount > 0`, telling the user how many attachments are in
  iCloud-only state and explaining the two manual routes (scroll-in-Messages
  to lazy-load, or the SIP/AMFI-off power-user mode).
- The original `purged.rs` / `ax.rs` modules and the per-chat UI were
  deleted on 2026-04-22 after verifying on a macOS 26.3 system that the
  "Download all attachments" button no longer exists in the Messages UI.
- `chat.db.transfer_state` and `ck_sync_state` are still read in the
  Preview step to compute the `notOnMacCount` value; we just don't act
  on them beyond informing the user.

## References

- [Beeper Barcelona — SIP/AMFI-off running notes](https://github.com/beeper/barcelona/blob/main/RUNNING.md)
- [BlueBubbles helper repo](https://github.com/BlueBubblesApp/bluebubbles-helper)
- [BlueBubbles IMCore docs](https://docs.bluebubbles.app/private-api/imcore-documentation)
- [ReagentX/imessage-exporter FAQ — manual "Download all attachments"](https://github.com/ReagentX/imessage-exporter/blob/develop/docs/faq.md)
- [The Apple Wiki — Dev:IMCore.framework](https://theapplewiki.com/wiki/Dev:IMCore.framework)
