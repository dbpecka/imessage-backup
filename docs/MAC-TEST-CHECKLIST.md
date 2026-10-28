# Manual test checklist (macOS)

Things the automated tests can't cover: they run on Linux against a
throwaway `chat.db`, so anything involving macOS permissions, code signing,
WebKit, real Messages data or iCloud needs a person and a Mac.

**Use test data.** Run this on a spare Mac user account (or a VM) signed in
to a **throwaway Apple ID**, with a second device on the same Apple ID for
the iCloud steps. Never run the delete steps against messages you care
about.

Record for each run: macOS version, Apple Silicon or Intel, build (dev,
signed, notarized), and the result of each step.

---

## 0. Setup

- [ ] Send yourself a mix of messages from the second device: plain text,
      emoji and smart quotes (`it’s`), a non-Latin script (e.g. `Привет`),
      two voice memos, a HEIC photo, a Live Photo, a sticker, a tapback on an
      older message, and a group chat.
- [ ] Note the total message count shown by Messages for one conversation.
- [ ] Quit Messages.

## 1. Full Disk Access

- [ ] First launch **without** Full Disk Access: the app shows the Full Disk
      Access guide (not "chat.db not found").
- [ ] "Open System Settings" lands on Privacy & Security → Full Disk Access,
      with Bubble Wrap already in the list.
- [ ] Turn it on and return to the app: it asks to relaunch, relaunches, and
      shows the conversation list with contact names.
- [ ] Revoke access while the app is open, then try a preview: the error is
      understandable (known gap: it doesn't route back to the guide).

## 2. Signed build (entitlements removed)

- [ ] Build, sign and notarize a release (`npm run tauri build`, then your
      usual notarization). `spctl -a -vv` accepts it.
- [ ] `codesign -d --entitlements - "Bubble Wrap.app"` shows **no**
      `disable-library-validation` and **no**
      `allow-unsigned-executable-memory`.
- [ ] The signed build launches, lists conversations, previews, backs up
      (JSON, HTML, PDF, TXT) and deletes. If anything fails, note the
      crash log — re-add only the specific entitlement it needs.

## 3. Content Security Policy

- [ ] `npm run tauri dev`: open the WebKit inspector (right-click → Inspect
      Element) → Console. No "Refused to…" CSP messages while you click
      through every step, including the orphan-cleanup dialog.
- [ ] Same check in the signed release build (enable the inspector with
      `defaults write io.pecka.apps.bubblewrap WebKitDeveloperExtras -bool true`
      if needed).

## 4. Backups

For each format (JSON, HTML, TXT; PDF last):

- [ ] Back up one conversation with "Copy attachment files" on, to a folder
      in your home folder. A new `Bubble Wrap <date>` subfolder appears.
- [ ] Both voice memos are present and play, with different contents.
- [ ] The HEIC photo and Live Photo are present byte-for-byte (compare with
      `shasum` against the files under `~/Library/Messages/Attachments`).
- [ ] Copied attachments keep their original dates in Finder.
- [ ] HTML: the conversation renders; note whether HEIC images display
      inline (known: browsers may not show HEIC).
- [ ] PDF: emoji/non-Latin text shows as `?`, and the result says PDF
      doesn't count as a backup for deleting.
- [ ] Back up the same conversation to an **external drive**: accepted, and
      the UI warns that the drive isn't protected.
- [ ] Back up into a folder inside `~/Library/Messages`: refused.
- [ ] Encrypted backup: tick "Encrypt", set a password, back up. A
      `.sparsebundle` appears; double-clicking it asks for the password and
      the backup is inside. Without the password it can't be opened.
- [ ] Not enough space: back up to a nearly full USB stick; the app refuses
      up front with a clear message instead of failing halfway.

## 5. Delete

- [ ] With no backup for the filter, Run delete is disabled until you tick
      "delete without a backup" **and** type `DELETE`.
- [ ] Back up (JSON), then preview a delete of one conversation for a past
      year. The step shows "Backed up in …" and names the conversation.
- [ ] Send a new message into that conversation (if the filter has no end
      date), then click Run delete: it's refused, the preview refreshes, and
      the new count is shown.
- [ ] Delete. Messages for that range are gone in Messages.app after
      relaunch; attachment files are gone from `~/Library/Messages/Attachments`;
      the result shows the snapshot path.
- [ ] Reopen Messages *during* a delete's snapshot step (large database
      helps): the delete is refused and no extra snapshot is left behind.
- [ ] "Attachments only" delete: the messages stay, the attachments are
      gone, and Messages shows the messages without broken bubbles (or note
      what it shows).

## 6. Restore from snapshot

- [ ] After a delete, open "Restore from snapshot", pick the snapshot from
      that delete and restore (Messages quit). Relaunch Messages: the
      deleted messages are back.
- [ ] A pre-restore snapshot of the state just before the restore exists.
- [ ] Restoring while Messages is running is refused.

## 7. Messages in iCloud (two devices, throwaway Apple ID)

This is the one behaviour the app can only infer from chat.db's triggers.

- [ ] With Messages in iCloud **on**, delete a few old messages on the Mac.
      Open Messages, wait for it to sync, and check the second device:
      **are the messages gone there too?** Record the answer — the app's
      warnings currently say deletions are "expected to" sync.
- [ ] Do they ever come back on the Mac after a sync?
- [ ] Orphan cleanup with iCloud on asks for the iCloud acknowledgement
      before removing records.

## 8. Orphan cleanup

- [ ] File → Clean Orphaned Data… with Messages quit: the scan finishes, the
      counts look plausible, and Live Photo videos next to their photos are
      *not* listed.
- [ ] Clean: the listed items are removed, nothing else in Messages breaks
      (spot-check a few conversations with attachments).
- [ ] With Messages running, cleaning is refused.

## 9. Large real database

- [ ] On a Mac with years of real history (read-only steps only: preview,
      backup, orphan **scan**): note how long the delete preview takes to
      update after changing the filter, and how long the orphan scan takes.
