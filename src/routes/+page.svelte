<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { open, confirm } from '@tauri-apps/plugin-dialog';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import {
    api,
    type ChatSummary,
    type ContactSummary,
    type BackupPreview,
    type FilterSpec,
    type ExportFormat,
    type BackupResult,
    type ProgressPayload,
    type SafetyStatus,
    type DeletePreview,
    type DeleteResult,
    type DeleteScope,
    type OrphanScan,
    type OrphanCleanResult,
    appErrorMessage,
    isFdaError
  } from '$lib/ipc';

  // FDA (Full Disk Access) gate. The app is unusable without it, so we
  // auto-detect on launch and on window focus (user returns from Settings).
  type FdaState = 'checking' | 'granted' | 'denied' | 'needs-restart' | 'missing-db';
  let fda = $state<FdaState>('checking');
  let fdaPath = $state<string>('');
  let fdaDetail = $state<string>('');
  let openingSettings = $state(false);
  // Sticky: true once we've observed a denial this session. Lets us force a
  // restart after the flip, since the previously-denied process may keep
  // serving cached TCC denials.
  let sawDenial = $state(false);

  let status = $state<'idle' | 'probing' | 'ok' | 'error'>('idle');
  let detail = $state<string>('');
  let chats = $state<ChatSummary[] | null>(null);
  let contacts = $state<ContactSummary[] | null>(null);
  let loadingChats = $state(false);
  let loadingContacts = $state(false);

  // filter state
  let startDate = $state('');
  let endDate = $state('');
  let selectedChatIds = $state<Set<number>>(new Set());
  let chatSearch = $state('');

  function parseDate(value: string, isEnd: boolean): { resolved: string; error: string } {
    const trimmed = value.trim();
    if (!trimmed) return { resolved: '', error: '' };

    if (/^\d{4}$/.test(trimmed)) {
      const year = parseInt(trimmed, 10);
      if (year < 1900 || year > 2100) return { resolved: '', error: 'Year out of range' };
      return { resolved: isEnd ? `${trimmed}-12-31` : `${trimmed}-01-01`, error: '' };
    }

    if (/^\d{4}-\d{2}$/.test(trimmed)) {
      const [y, m] = trimmed.split('-').map(Number);
      if (m < 1 || m > 12) return { resolved: '', error: 'Invalid month' };
      if (isEnd) {
        const lastDay = new Date(y, m, 0).getDate();
        return { resolved: `${trimmed}-${String(lastDay).padStart(2, '0')}`, error: '' };
      }
      return { resolved: `${trimmed}-01`, error: '' };
    }

    if (/^\d{4}-\d{2}-\d{2}$/.test(trimmed)) {
      const [y, m, d] = trimmed.split('-').map(Number);
      const date = new Date(trimmed + 'T00:00:00');
      if (
        isNaN(date.getTime()) ||
        date.getUTCFullYear() !== y ||
        date.getUTCMonth() + 1 !== m ||
        date.getUTCDate() !== d
      ) {
        return { resolved: '', error: 'Invalid date' };
      }
      return { resolved: trimmed, error: '' };
    }

    return { resolved: '', error: 'Use YYYY-MM-DD' };
  }

  let startDateParsed = $derived(parseDate(startDate, false));
  let endDateParsed = $derived(parseDate(endDate, true));
  let endDateOrderError = $derived(
    !startDateParsed.error &&
      !endDateParsed.error &&
      !!startDateParsed.resolved &&
      !!endDateParsed.resolved &&
      endDateParsed.resolved < startDateParsed.resolved
      ? 'End date must be after start date'
      : ''
  );

  // preview state
  let preview = $state<BackupPreview | null>(null);
  let previewing = $state(false);
  let previewError = $state<string>('');
  let previewSeq = 0;
  let previewTimer: ReturnType<typeof setTimeout> | undefined;

  // backup state
  let format = $state<ExportFormat>('json');
  let destination = $state<string>('');
  let copyAttachments = $state(true);
  let running = $state(false);
  let runError = $state<string>('');
  let runResult = $state<BackupResult | null>(null);
  let progress = $state<ProgressPayload | null>(null);
  let unlistenProgress: UnlistenFn | null = null;

  // orphan clean state (driven by File > Clean Orphaned Data… menu item)
  type OrphanModalState = 'scanning' | 'preview' | 'cleaning' | 'done' | 'error';
  let orphanModal = $state<OrphanModalState | null>(null);
  let orphanScan = $state<OrphanScan | null>(null);
  let orphanResult = $state<OrphanCleanResult | null>(null);
  let orphanError = $state('');
  let unlistenMenuClean: UnlistenFn | null = null;
  let orphanModalCard = $state<HTMLDivElement | null>(null);

  // Modal can be dismissed by the user only when no destructive work is in
  // flight. 'cleaning' is a point of no return; 'scanning' is cancel-safe in
  // principle but the UI doesn't wire a cancellation path, so gate it too.
  const orphanModalDismissable = $derived(
    orphanModal === 'preview' || orphanModal === 'done' || orphanModal === 'error'
  );

  function closeOrphanModal() {
    if (orphanModalDismissable) orphanModal = null;
  }

  // Focus trap: keep Tab inside the dialog while it's open and route Escape
  // to the dismiss path. Autofocus the first actionable control on open.
  $effect(() => {
    if (orphanModal === null) return;
    queueMicrotask(() => {
      const first = orphanModalCard?.querySelector<HTMLElement>(
        'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])'
      );
      first?.focus();
    });
  });

  function onOrphanModalKeydown(e: KeyboardEvent) {
    if (orphanModal === null) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      closeOrphanModal();
      return;
    }
    if (e.key !== 'Tab' || !orphanModalCard) return;
    const focusables = Array.from(
      orphanModalCard.querySelectorAll<HTMLElement>(
        'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])'
      )
    ).filter((el) => !el.hasAttribute('disabled'));
    if (focusables.length === 0) return;
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    const active = document.activeElement as HTMLElement | null;
    if (e.shiftKey && active === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && active === last) {
      e.preventDefault();
      first.focus();
    }
  }

  // delete state
  let deleteExpanded = $state(false);
  let safety = $state<SafetyStatus | null>(null);
  let deletePreview = $state<DeletePreview | null>(null);
  let deletePreviewing = $state(false);
  let deletePreviewSeq = 0;
  let deletePreviewTimer: ReturnType<typeof setTimeout> | undefined;
  let deleteMessages = $state(true);
  let deleteAttachments = $state(true);
  let deleting = $state(false);
  let deleteResult = $state<DeleteResult | null>(null);
  let deleteError = $state('');
  let icloudAcknowledged = $state(false);
  let skipBackupAcknowledged = $state(false);

  let unlistenFocus: UnlistenFn | null = null;

  onMount(() => {
    checkFda();
    getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        // User likely just came back from System Settings. Re-check only
        // while we're still gating — once granted/restarted, stop polling.
        if (focused && (fda === 'denied' || fda === 'checking')) {
          checkFda();
        }
      })
      .then((fn) => {
        unlistenFocus = fn;
      });
    listen<ProgressPayload>('backup-progress', (e) => {
      progress = e.payload;
    }).then((fn) => {
      unlistenProgress = fn;
    });
    listen('menu:clean-orphans', startOrphanScan).then((fn) => {
      unlistenMenuClean = fn;
    });
  });

  onDestroy(() => {
    unlistenProgress?.();
    unlistenFocus?.();
    unlistenMenuClean?.();
  });

  async function checkFda() {
    try {
      const res = await api.checkFda();
      fdaPath = res.path;
      if (res.granted) {
        // If we saw a denial earlier in this session, force a relaunch.
        // Otherwise the running process may still be using a cached TCC
        // denial and fail when it tries to open chat.db via SQLite.
        fda = sawDenial ? 'needs-restart' : 'granted';
      } else {
        sawDenial = true;
        fda = 'denied';
      }
    } catch (err) {
      fda = 'missing-db';
      fdaDetail = appErrorMessage(err);
    }
  }

  async function openFdaSettings() {
    openingSettings = true;
    try {
      await api.openFdaSettings();
    } catch (err) {
      fdaDetail = appErrorMessage(err);
    } finally {
      openingSettings = false;
    }
  }

  async function relaunch() {
    try {
      await api.relaunchApp();
    } catch (err) {
      fdaDetail = appErrorMessage(err);
    }
  }

  // Auto-probe as soon as FDA flips to granted, so the user doesn't have to
  // click a second button after the OS stopped blocking us.
  $effect(() => {
    if (fda === 'granted' && status === 'idle') {
      probe();
    }
  });

  async function probe() {
    status = 'probing';
    detail = '';
    try {
      const result = await api.probeDb();
      status = 'ok';
      detail = `${result.messageCount.toLocaleString()} messages found`;
      if (!chats) await loadChats();
    } catch (err) {
      // SQLite opens via a different path than File::open, so it can still
      // report FDA denial even when the lightweight check succeeded. Route
      // back to the gate so the user gets the restart prompt.
      if (isFdaError(err)) {
        sawDenial = true;
        fda = 'denied';
        status = 'idle';
        return;
      }
      status = 'error';
      detail = appErrorMessage(err);
    }
  }

  async function loadChats() {
    loadingChats = true;
    try {
      chats = await api.listChats();
    } catch (err) {
      detail = appErrorMessage(err);
    } finally {
      loadingChats = false;
    }
  }

  async function loadContacts() {
    loadingContacts = true;
    try {
      contacts = await api.listContacts();
    } catch (err) {
      detail = appErrorMessage(err);
    } finally {
      loadingContacts = false;
    }
  }

  function toggleChat(rowid: number) {
    const next = new Set(selectedChatIds);
    if (next.has(rowid)) next.delete(rowid);
    else next.add(rowid);
    selectedChatIds = next;
  }

  function clearChats() {
    selectedChatIds = new Set();
  }

  function buildFilter(): FilterSpec {
    const filter: FilterSpec = {};
    const start = startDateParsed.resolved;
    const end = endDateParsed.resolved;
    if (start || end) {
      filter.dateRange = { start: start || undefined, end: end || undefined };
    }
    if (selectedChatIds.size > 0) {
      filter.chatIds = [...selectedChatIds];
    }
    return filter;
  }

  async function runPreview() {
    const seq = ++previewSeq;
    previewing = true;
    previewError = '';
    try {
      const result = await api.previewBackup(buildFilter());
      if (seq === previewSeq) {
        preview = result;
      }
    } catch (err) {
      if (seq === previewSeq) {
        previewError = appErrorMessage(err);
        preview = null;
      }
    } finally {
      if (seq === previewSeq) {
        previewing = false;
      }
    }
  }

  // Auto-refresh preview whenever filter state changes (debounced 400 ms).
  $effect(() => {
    void startDate;
    void endDate;
    void selectedChatIds;

    if (status !== 'ok') return;
    if (startDateParsed.error || endDateParsed.error || endDateOrderError) return;

    clearTimeout(previewTimer);
    previewTimer = setTimeout(runPreview, 400);

    return () => clearTimeout(previewTimer);
  });

  // When filters change, any prior `runResult` no longer covers the current
  // selection. Invalidate the backup-verified signal so the delete gate
  // forces a fresh backup (or an explicit skip ack) that actually matches
  // what's about to be deleted. This is defense-in-depth — the backend
  // can't tell if a backup was taken for a different filter.
  $effect(() => {
    void startDate;
    void endDate;
    void selectedChatIds;
    runResult = null;
    skipBackupAcknowledged = false;
  });

  async function pickDestination() {
    const picked = await open({
      directory: true,
      multiple: false,
      title: 'Choose a folder to back up iMessages into'
    });
    if (typeof picked === 'string') destination = picked;
  }

  async function runBackup() {
    runError = '';
    runResult = null;
    progress = null;
    if (!destination) {
      runError = 'Pick a destination folder first.';
      return;
    }
    running = true;
    try {
      runResult = await api.runBackup({
        filter: buildFilter(),
        format,
        destination,
        copyAttachments
      });
    } catch (err) {
      runError = appErrorMessage(err);
    } finally {
      running = false;
    }
  }

  let checkingSafety = $state(false);

  async function refreshSafety(): Promise<boolean> {
    checkingSafety = true;
    try {
      safety = await api.safetyStatus();
      return true;
    } catch (err) {
      deleteError = appErrorMessage(err);
      safety = null;
      return false;
    } finally {
      checkingSafety = false;
    }
  }

  async function expandDelete() {
    deleteError = '';
    const ok = await refreshSafety();
    if (!ok) {
      // Leave the section collapsed so the error isn't buried under empty
      // delete UI; refreshSafety() has already populated `deleteError`.
      return;
    }
    deleteExpanded = true;
  }

  async function runDeletePreview() {
    const seq = ++deletePreviewSeq;
    deletePreviewing = true;
    deleteError = '';
    try {
      const result = await api.previewDelete(buildFilter());
      if (seq === deletePreviewSeq) deletePreview = result;
    } catch (err) {
      if (seq === deletePreviewSeq) {
        deleteError = appErrorMessage(err);
        deletePreview = null;
      }
    } finally {
      if (seq === deletePreviewSeq) deletePreviewing = false;
    }
  }

  // Auto-refresh delete preview when the section is open and filters change.
  $effect(() => {
    void startDate;
    void endDate;
    void selectedChatIds;
    void deleteExpanded;

    if (status !== 'ok' || !deleteExpanded) return;

    clearTimeout(deletePreviewTimer);
    deletePreviewTimer = setTimeout(runDeletePreview, 400);
    return () => clearTimeout(deletePreviewTimer);
  });

  async function runDelete() {
    if (!deletePreview) return;

    const parts: string[] = [];
    if (deleteMessages && deletePreview.messageCount > 0)
      parts.push(
        `${deletePreview.messageCount.toLocaleString()} message${deletePreview.messageCount === 1 ? '' : 's'}`
      );
    if (deleteAttachments && deletePreview.attachmentCount > 0)
      parts.push(
        `${deletePreview.attachmentCount.toLocaleString()} attachment${deletePreview.attachmentCount === 1 ? '' : 's'}${deletePreview.attachmentBytes > 0 ? ` (${formatBytes(deletePreview.attachmentBytes)})` : ''}`
      );

    const ok = await confirm(
      `Permanently delete ${parts.join(' and ')} from chat.db? This cannot be undone.`,
      { title: 'Confirm Delete', kind: 'warning' }
    );
    if (!ok) return;

    deleting = true;
    deleteError = '';
    deleteResult = null;
    try {
      // Final safety probe: Messages.app could have reopened between the
      // preview and the click.
      const safetyOk = await refreshSafety();
      if (!safetyOk) {
        // refreshSafety already populated deleteError.
        return;
      }
      if (safety?.messagesRunning) {
        deleteError =
          'The Messages app is running again — quit it and click Re-check before retrying.';
        return;
      }
      deleteResult = await api.runDelete({
        filter: buildFilter(),
        confirmationPhrase: 'DELETE',
        backupVerified: runResult !== null,
        acknowledgeSkipBackup: runResult !== null || skipBackupAcknowledged,
        deleteScope,
        // Surface the iCloud risk to the backend so it can refuse if detection
        // flipped to Enabled since the user ticked the acknowledgement box.
        acknowledgeIcloudSync: safety?.icloudMessages !== 'enabled' || icloudAcknowledged
      });
      await Promise.all([runPreview(), runDeletePreview(), loadChats()]);
    } catch (err) {
      deleteError = appErrorMessage(err);
    } finally {
      deleting = false;
    }
  }

  const deleteScope = $derived<DeleteScope>(
    deleteMessages && deleteAttachments
      ? 'both'
      : deleteMessages
        ? 'messages_only'
        : 'attachments_only'
  );

  async function startOrphanScan() {
    orphanModal = 'scanning';
    orphanScan = null;
    orphanResult = null;
    orphanError = '';
    try {
      orphanScan = await api.scanOrphans();
      orphanModal = 'preview';
    } catch (err) {
      orphanError = appErrorMessage(err);
      orphanModal = 'error';
    }
  }

  async function runOrphanClean() {
    orphanModal = 'cleaning';
    orphanError = '';
    try {
      orphanResult = await api.cleanOrphans();
      orphanModal = 'done';
    } catch (err) {
      orphanError = appErrorMessage(err);
      orphanModal = 'error';
    }
  }

  const totalOrphanItems = $derived(
    (orphanScan?.dbOrphanCount ?? 0) + (orphanScan?.fsOrphanCount ?? 0)
  );
  const totalOrphanBytes = $derived(
    (orphanScan?.dbOrphanBytes ?? 0) + (orphanScan?.fsOrphanBytes ?? 0)
  );

  const deleteReady = $derived(
    deletePreview !== null &&
      (deleteMessages || deleteAttachments) &&
      ((deleteMessages && deletePreview.messageCount > 0) ||
        (deleteAttachments && deletePreview.attachmentCount > 0)) &&
      safety !== null &&
      !safety.messagesRunning &&
      (safety.icloudMessages !== 'enabled' || icloudAcknowledged) &&
      (runResult !== null || skipBackupAcknowledged) &&
      !deleting
  );

  function displayName(c: ChatSummary) {
    if (c.contactName) return c.contactName;
    if (c.displayName) return c.displayName;
    // For group chats with no assigned name, chatIdentifier is an opaque
    // `chatNNN` — fall back to the participants' phone numbers / emails.
    if (c.chatIdentifier && !/^chat\d+$/.test(c.chatIdentifier)) return c.chatIdentifier;
    if (c.participantHandles.length > 0) return c.participantHandles.join(', ');
    return c.chatIdentifier || `Chat ${c.rowid}`;
  }

  function formatBytes(n: number) {
    if (n < 1024) return `${n} B`;
    if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
    return `${(n / 1024 ** 3).toFixed(2)} GB`;
  }

  const filteredChats = $derived.by(() => {
    if (!chats) return [] as ChatSummary[];
    if (!chatSearch.trim()) return chats;
    const q = chatSearch.toLowerCase();
    return chats.filter(
      (c) =>
        displayName(c).toLowerCase().includes(q) ||
        c.chatIdentifier.toLowerCase().includes(q) ||
        (c.serviceName ?? '').toLowerCase().includes(q)
    );
  });

  const progressPct = $derived(
    progress && progress.total > 0
      ? Math.min(100, Math.round((progress.position / progress.total) * 100))
      : 0
  );

  const fdaCollapsed = $derived(fda === 'granted' && status === 'ok');
</script>

<section class="card" class:collapsed={fdaCollapsed}>
  <header class="card-head">
    <span class="step">1</span>
    <h2>Permissions</h2>
    {#if fdaCollapsed}
      <span class="check-badge" aria-label="Granted" title="Granted">
        <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
          <path
            d="M3 8.5l3.2 3.2L13 5"
            fill="none"
            stroke="currentColor"
            stroke-width="2.2"
            stroke-linecap="round"
            stroke-linejoin="round"
          />
        </svg>
      </span>
    {/if}
  </header>

  {#if fdaCollapsed}
    <!-- Granted and probed — no action required, collapsed. -->
  {:else if fda === 'checking'}
    <p class="meta">Checking Full Disk Access…</p>
  {:else if fda === 'missing-db'}
    <p class="err">{fdaDetail}</p>
    <div class="actions">
      <button onclick={checkFda}>Check again</button>
    </div>
  {:else if fda === 'denied'}
    <p>
      BubbleWrap needs Full Disk Access to read your iMessages. Open System
      Settings and enable <strong>Bubble Wrap</strong> under
      <strong>Privacy &amp; Security → Full Disk Access</strong>.
    </p>
    <div class="actions">
      <button class="primary" onclick={openFdaSettings} disabled={openingSettings}>
        {openingSettings ? 'Opening…' : 'Open Full Disk Access settings'}
      </button>
      <button onclick={checkFda}>I&apos;ve granted it — check again</button>
    </div>
  {:else if fda === 'needs-restart'}
    <p>
      Access granted. Please relaunch the app and we'll be good to go!
    </p>
    <div class="actions">
      <button class="primary" onclick={relaunch}>Restart app</button>
    </div>
  {:else}
    <div class="actions">
      <button onclick={probe} disabled={status === 'probing'}>
        {status === 'probing' ? 'Probing…' : 'Check Again'}
      </button>
      {#if status === 'error'}
        <span class="pill err">{detail}</span>
      {/if}
    </div>
  {/if}
</section>

{#if status === 'ok'}
  <section class="card">
    <header class="card-head">
      <span class="step">2</span>
      <h2>Filters</h2>
    </header>

    <div class="field">
      <label class:date-invalid={!!startDateParsed.error}>
        Start date
        <div class="date-wrap">
          <input
            type="text"
            placeholder="YYYY, YYYY-MM, or YYYY-MM-DD"
            bind:value={startDate}
            aria-describedby={startDateParsed.error ? 'start-date-err' : undefined}
            aria-invalid={!!startDateParsed.error}
          />
          <span class="cal-icon" aria-hidden="true">
            <svg width="14" height="14" viewBox="0 0 14 14" fill="none" xmlns="http://www.w3.org/2000/svg">
              <rect x="0.6" y="2.6" width="12.8" height="10.8" rx="1.4" stroke="currentColor" stroke-width="1.2"/>
              <path d="M0.6 6h12.8" stroke="currentColor" stroke-width="1.2"/>
              <path d="M4.5 0.5v2.5M9.5 0.5v2.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>
            </svg>
          </span>
        </div>
        {#if startDateParsed.error}<span id="start-date-err" class="date-err">{startDateParsed.error}</span>{/if}
      </label>
      <label class:date-invalid={!!(endDateParsed.error || endDateOrderError)}>
        End date
        <div class="date-wrap">
          <input
            type="text"
            placeholder="YYYY, YYYY-MM, or YYYY-MM-DD"
            bind:value={endDate}
            aria-describedby={endDateParsed.error || endDateOrderError ? 'end-date-err' : undefined}
            aria-invalid={!!(endDateParsed.error || endDateOrderError)}
          />
          <span class="cal-icon" aria-hidden="true">
            <svg width="14" height="14" viewBox="0 0 14 14" fill="none" xmlns="http://www.w3.org/2000/svg">
              <rect x="0.6" y="2.6" width="12.8" height="10.8" rx="1.4" stroke="currentColor" stroke-width="1.2"/>
              <path d="M0.6 6h12.8" stroke="currentColor" stroke-width="1.2"/>
              <path d="M4.5 0.5v2.5M9.5 0.5v2.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>
            </svg>
          </span>
        </div>
        {#if endDateParsed.error || endDateOrderError}<span id="end-date-err" class="date-err">{endDateParsed.error || endDateOrderError}</span>{/if}
      </label>
    </div>

    <div class="field">
      <label class="grow">
        Chats
        <input
          type="search"
          placeholder="Search chats…"
          bind:value={chatSearch}
          disabled={!chats}
        />
      </label>
      <div class="actions">
        <button onclick={loadChats} disabled={loadingChats}>
          {loadingChats ? 'Loading…' : chats ? 'Reload' : 'Load chats'}
        </button>
        <button onclick={clearChats} disabled={selectedChatIds.size === 0}>
          Clear ({selectedChatIds.size})
        </button>
      </div>
    </div>

    {#if chats}
      <ul class="scroll">
        {#each filteredChats.slice(0, 200) as chat (chat.rowid)}
          <li>
            <label>
              <input
                type="checkbox"
                checked={selectedChatIds.has(chat.rowid)}
                onchange={() => toggleChat(chat.rowid)}
              />
              <span class="name">{displayName(chat)}</span>
              <span class="meta">
                {chat.messageCount.toLocaleString()} msgs
                · {chat.participantCount} participant{chat.participantCount === 1 ? '' : 's'}
                {#if chat.serviceName}· {chat.serviceName}{/if}
              </span>
            </label>
          </li>
        {/each}
      </ul>
      {#if filteredChats.length > 200}
        <p class="meta">Showing first 200 of {filteredChats.length} matching.</p>
      {/if}
    {/if}
  </section>

  <section class="card">
    <header class="card-head">
      <span class="step">3</span>
      <h2>Preview</h2>
    </header>
    {#if previewing}
      <p class="meta preview-counting">Counting…</p>
    {:else if preview}
      <div class="preview-result">
        <span class="preview-count">{preview.messageCount.toLocaleString()}</span>
        <span class="preview-label">messages</span>
        {#if preview.attachmentCount > 0}
          <span class="preview-sep">·</span>
          {#if preview.attachmentBytes > 0}
            <span class="preview-count">{formatBytes(preview.attachmentBytes)}</span>
            <span class="preview-label">
              ({preview.attachmentCount.toLocaleString()} attachment{preview.attachmentCount === 1 ? '' : 's'})
            </span>
          {:else}
            <span class="preview-count">{preview.attachmentCount.toLocaleString()}</span>
            <span class="preview-label">attachment{preview.attachmentCount === 1 ? '' : 's'}</span>
          {/if}
          {#if preview.notOnMacCount > 0 || preview.missingCount > 0}
            <span class="preview-sep">·</span>
            <span class="preview-label not-on-mac-warn">
              {preview.onDiskCount.toLocaleString()} on disk
              {#if preview.notOnMacCount > 0}· {preview.notOnMacCount.toLocaleString()} not on this Mac{/if}
              {#if preview.missingCount > 0}· {preview.missingCount.toLocaleString()} missing{/if}
            </span>
          {/if}
        {:else if preview.hasFilters}
          <span class="preview-sep">·</span>
          <span class="preview-label">no attachments</span>
        {/if}
        <span class="pill {preview.hasFilters ? 'pill-filter' : 'pill-all'}">
          {preview.hasFilters ? 'filtered' : 'entire database'}
        </span>
      </div>
    {:else}
      <p class="meta">Waiting for filter…</p>
    {/if}
    {#if previewError}
      <p class="err">{previewError}</p>
    {/if}

    {#if preview && preview.notOnMacCount > 0}
      <div class="icloud-notice">
        <div class="icloud-notice-title">Only locally-stored attachments will be backed up</div>
        <p class="meta">
          {preview.onDiskCount.toLocaleString()} of {(preview.attachmentCount).toLocaleString()} attachments are on this Mac.
          {preview.notOnMacCount.toLocaleString()} attachment{preview.notOnMacCount === 1 ? '' : 's'} live in iCloud and can't be
          included in the backup unless they're re-downloaded first.
        </p>
        <p class="meta">
          To include them: Open Messages, scroll through each chat you care about so the
          purged photos and files render. When the counts above stabilise, re-run the preview and the
          backup will pick them up.
        </p>
      </div>
    {/if}
  </section>

  <section class="card">
    <header class="card-head">
      <span class="step">4</span>
      <h2>Backup</h2>
    </header>

    <div class="field">
      <label>
        Format
        <select bind:value={format} disabled={running}>
          <option value="json">JSON (line-delimited, one file per conversation)</option>
          <option value="html">HTML (self-contained per conversation)</option>
          <option value="pdf">PDF (one file per conversation)</option>
          <option value="txt">TXT (plain text per conversation)</option>
        </select>
      </label>
      <label class="grow">
        Destination
        <div class="inline">
          <input
            type="text"
            placeholder="Pick a folder…"
            bind:value={destination}
            readonly
            disabled={running}
          />
          <button onclick={pickDestination} disabled={running}>Choose…</button>
        </div>
      </label>
    </div>

    <label class="checkbox">
      <input type="checkbox" bind:checked={copyAttachments} disabled={running} />
      Copy attachment files into the backup folder
    </label>

    <div class="actions">
      <button class="primary" onclick={runBackup} disabled={running || !destination}>
        {running ? 'Running…' : 'Run backup'}
      </button>
    </div>

    {#if progress && !runResult}
      <div
        class="progress"
        role="progressbar"
        aria-valuenow={progressPct}
        aria-valuemin="0"
        aria-valuemax="100"
        aria-describedby="backup-progress-desc"
        aria-label="Backup progress"
      >
        <div class="bar" style="width: {progressPct}%"></div>
      </div>
      <p id="backup-progress-desc" class="meta">
        {progress.message} · {progress.position.toLocaleString()} / {progress.total.toLocaleString()}
        ({progressPct}%)
      </p>
    {/if}

    {#if runError}
      <p class="err">{runError}</p>
    {/if}

    {#if runResult}
      <div class="summary">
        <p class="ok">Backup complete — {runResult.format.toUpperCase()}.</p>
        <ul>
          <li>{runResult.messageCount.toLocaleString()} messages</li>
          <li>{runResult.conversationCount.toLocaleString()} conversations</li>
          <li>
            {runResult.attachmentCount.toLocaleString()} attachments
            {#if runResult.attachmentBytesCopied > 0}
              ({formatBytes(runResult.attachmentBytesCopied)} copied)
            {/if}
          </li>
          {#if runResult.manifestPath}
            <li>
              Manifest: <code>{runResult.manifestPath}</code>
            </li>
          {/if}
        </ul>
      </div>
    {/if}
  </section>

  <section class="card danger">
    <header class="card-head">
      <span class="step step-danger">5</span>
      <h2>Delete from iMessage <span class="optional">(optional)</span></h2>
    </header>
    {#if !deleteExpanded}
      <p class="meta">
        Permanently remove messages matching the filter above from the Messages database. A
        snapshot of <code>chat.db</code> is created first. This is destructive.
      </p>
      <button onclick={expandDelete}>Show delete controls</button>
    {:else}
      {#if safety?.messagesRunning}
        <p class="err" style="display:flex;align-items:center;justify-content:space-between;gap:0.5rem;">
          <span>The Messages app is currently running. Please quit it before deleting messages.</span>
          <button onclick={refreshSafety} disabled={checkingSafety} style="flex-shrink:0;">
            {checkingSafety ? 'Checking…' : 'Re-check'}
          </button>
        </p>
      {:else if safety}
        <p class="meta" style="display:flex;align-items:center;justify-content:space-between;gap:0.5rem;">
          <span><strong>Messages app</strong>: Not running.</span>
          <span class="check-badge" aria-label="Not running" title="Messages.app is not running">
            <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
              <path
                d="M3 8.5l3.2 3.2L13 5"
                fill="none"
                stroke="currentColor"
                stroke-width="2.2"
                stroke-linecap="round"
                stroke-linejoin="round"
              />
            </svg>
          </span>
        </p>
      {/if}
      {#if safety?.icloudMessages === 'enabled'}
        <p class="err">
          <strong>Messages in iCloud is enabled.</strong> Deletes on this Mac will replicate to all
          your other devices signed into the same Apple ID. Disable it in
          <strong>System Settings → [your name] → iCloud → Show More Apps → Messages</strong> if you
          want local-only deletion.
        </p>
        <label class="checkbox">
          <input type="checkbox" bind:checked={icloudAcknowledged} disabled={deleting} />
          I understand deletes will sync to my other devices and want to proceed anyway.
        </label>
      {:else if safety?.icloudMessages === 'unknown'}
        <p class="meta">
          Couldn't confirm Messages-in-iCloud state. If it's enabled, deletes will replicate to your
          other devices.
        </p>
      {/if}

      <div>
        <label class="checkbox">
          <input type="checkbox" bind:checked={deleteMessages} disabled={deleting} />
          Delete messages
        </label>
        <label class="checkbox">
          <input type="checkbox" bind:checked={deleteAttachments} disabled={deleting} />
          Delete attachment files
        </label>
      </div>

      {#if deletePreviewing}
        <p class="meta preview-counting">Counting…</p>
      {:else if deletePreview}
        {#if deleteMessages && deleteAttachments}
          <p>
            Will delete <strong>{deletePreview.messageCount.toLocaleString()}</strong> messages and
            <strong>{deletePreview.attachmentCount.toLocaleString()}</strong> attachments
            ({formatBytes(deletePreview.attachmentBytes)} on disk across
            {deletePreview.onDiskFileCount.toLocaleString()} files).
          </p>
        {:else if deleteMessages}
          <p>
            Will delete <strong>{deletePreview.messageCount.toLocaleString()}</strong> messages.
            Attachment records and files will be left on disk.
          </p>
        {:else if deleteAttachments}
          <p>
            Will delete <strong>{deletePreview.attachmentCount.toLocaleString()}</strong> attachment
            files ({formatBytes(deletePreview.attachmentBytes)} across
            {deletePreview.onDiskFileCount.toLocaleString()} files) from
            {deletePreview.messageCount.toLocaleString()} messages. Message text will be preserved.
          </p>
        {:else}
          <p class="meta">Select at least one option above.</p>
        {/if}

        {#if !runResult}
          <p class="meta">
            Tip: run a backup first (Step 4). Without a backup you have no way to restore the data
            you're about to remove.
          </p>
          <label class="checkbox">
            <input
              type="checkbox"
              bind:checked={skipBackupAcknowledged}
              disabled={deleting}
            />
            I understand and want to delete without a backup.
          </label>
        {/if}

        <div class="actions">
          <button class="danger" onclick={runDelete} disabled={!deleteReady}>
            {deleting ? 'Deleting…' : 'Run delete'}
          </button>
        </div>
      {/if}

      {#if deleteError}
        <p class="err">{deleteError}</p>
      {/if}

      {#if deleteResult}
        <div class="summary">
          <p class="ok">Delete complete.</p>
          <ul>
            <li>{deleteResult.messagesDeleted.toLocaleString()} messages</li>
            <li>
              {deleteResult.attachmentsDeleted.toLocaleString()} attachments
              ({deleteResult.onDiskFilesRemoved.toLocaleString()} files removed from disk{#if deleteResult.onDiskFilesFailed > 0}
                , {deleteResult.onDiskFilesFailed.toLocaleString()} failed
              {/if})
            </li>
            <li>
              {deleteResult.chatMessageJoinsDeleted.toLocaleString()} chat-message join rows
              · {deleteResult.attachmentJoinsDeleted.toLocaleString()} attachment join rows
            </li>
            {#if deleteResult.orphanChatsDeleted > 0 || deleteResult.orphanHandlesDeleted > 0}
              <li>
                Orphans: {deleteResult.orphanChatsDeleted.toLocaleString()} chats,
                {deleteResult.orphanHandlesDeleted.toLocaleString()} handles
              </li>
            {/if}
            <li>
              Snapshot: <code>{deleteResult.snapshotPath}</code>
            </li>
          </ul>
        </div>
      {/if}
    {/if}
  </section>

{/if}

<svelte:window onkeydown={onOrphanModalKeydown} />

<!-- ─── Orphan-clean modal (triggered by File > Clean Orphaned Data…) ─── -->
{#if orphanModal !== null}
  <div
    class="modal-back"
    role="dialog"
    aria-modal="true"
    aria-label="Clean Orphaned Data"
    onclick={closeOrphanModal}
  >
    <div class="modal-card" bind:this={orphanModalCard} onclick={(e) => e.stopPropagation()}>
      <h3 class="modal-title">Clean Orphaned Data</h3>

      {#if orphanModal === 'scanning'}
        <p class="meta">Scanning…</p>

      {:else if orphanModal === 'preview' && orphanScan}
        <div class="orphan-rows">
          <div class="orphan-row">
            <span class="orphan-label">Unlinked attachment records</span>
            <span class="orphan-count">{orphanScan.dbOrphanCount.toLocaleString()}</span>
            <span class="orphan-size">{formatBytes(orphanScan.dbOrphanBytes)}</span>
          </div>
          <div class="orphan-row">
            <span class="orphan-label">Untracked files on disk</span>
            <span class="orphan-count">{orphanScan.fsOrphanCount.toLocaleString()}</span>
            <span class="orphan-size">{formatBytes(orphanScan.fsOrphanBytes)}</span>
          </div>
        </div>
        <p class="orphan-total">
          {#if totalOrphanItems === 0}
            No orphaned data found.
          {:else}
            <strong>{totalOrphanItems.toLocaleString()} item{totalOrphanItems === 1 ? '' : 's'}</strong>
            · {formatBytes(totalOrphanBytes)} recoverable
          {/if}
        </p>
        <div class="actions">
          {#if totalOrphanItems > 0}
            <button class="danger" onclick={runOrphanClean}>
              Delete {totalOrphanItems.toLocaleString()} item{totalOrphanItems === 1 ? '' : 's'}
            </button>
          {/if}
          <button onclick={() => (orphanModal = null)}>
            {totalOrphanItems === 0 ? 'Close' : 'Cancel'}
          </button>
        </div>

      {:else if orphanModal === 'cleaning'}
        <p class="meta">Cleaning up…</p>

      {:else if orphanModal === 'done' && orphanResult}
        {@const filesRemoved = orphanResult.dbFilesRemoved + orphanResult.fsFilesRemoved}
        {@const filesFailed = orphanResult.dbFilesFailed + orphanResult.fsFilesFailed}
        <div class="summary">
          <p class="ok">Done.</p>
          <ul>
            {#if orphanResult.dbRowsDeleted > 0}
              <li>{orphanResult.dbRowsDeleted.toLocaleString()} database records removed</li>
            {/if}
            {#if filesRemoved > 0 || filesFailed > 0}
              <li>
                {filesRemoved.toLocaleString()} file{filesRemoved === 1 ? '' : 's'} removed from
                disk{#if filesFailed > 0}, {filesFailed.toLocaleString()} failed{/if}
              </li>
            {/if}
          </ul>
        </div>
        <div class="actions">
          <button onclick={() => (orphanModal = null)}>Close</button>
        </div>

      {:else if orphanModal === 'error'}
        <p class="err">{orphanError}</p>
        <div class="actions">
          <button onclick={() => (orphanModal = null)}>Close</button>
        </div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .lede {
    color: var(--text-muted);
    margin: 0 0 30px;
    font-size: 14px;
  }

  .card {
    margin-top: 18px;
    padding: 20px 22px;
    border-radius: var(--radius-lg);
    background: var(--surface);
    border: 1px solid var(--surface-ring);
    box-shadow: var(--shadow-card);
    backdrop-filter: saturate(160%) blur(16px);
    -webkit-backdrop-filter: saturate(160%) blur(16px);
    animation: card-in 360ms cubic-bezier(0.22, 1, 0.36, 1) both;
  }

  @keyframes card-in {
    from {
      opacity: 0;
      transform: translateY(6px);
    }
    to {
      opacity: 1;
      transform: none;
    }
  }

  .card-head {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 8px;
  }

  .card-head h2 {
    margin: 0;
    font-size: 15px;
    font-weight: 600;
    letter-spacing: -0.005em;
  }

  .optional {
    color: var(--text-subtle);
    font-weight: 400;
  }

  .step {
    display: inline-grid;
    place-items: center;
    width: 22px;
    height: 22px;
    border-radius: 50%;
    background: var(--accent-soft);
    color: var(--accent);
    font-size: 12px;
    font-weight: 600;
  }

  .step-danger {
    background: var(--danger-soft);
    color: var(--danger);
  }

  .check-badge {
    margin-left: auto;
    display: inline-grid;
    place-items: center;
    width: 22px;
    height: 22px;
    border-radius: 50%;
    background: var(--success-soft);
    color: var(--success);
  }

  .card.collapsed {
    padding-top: 14px;
    padding-bottom: 14px;
  }
  .card.collapsed .card-head {
    margin-bottom: 0;
  }

  .card p {
    margin: 6px 0 10px;
    color: var(--text-muted);
  }

  .field {
    display: flex;
    gap: 12px;
    align-items: end;
    flex-wrap: wrap;
    margin: 10px 0;
  }
  .field label {
    display: flex;
    flex-direction: column;
    gap: 5px;
    font-size: 12.5px;
    color: var(--text-muted);
    font-weight: 500;
  }
  .field label.grow {
    flex: 1;
    min-width: 260px;
  }
  .field input[type='search'],
  .field input[type='text'],
  .field select {
    padding: 7px 10px;
    font: inherit;
    color: var(--text);
    border-radius: var(--radius-sm);
    border: 1px solid var(--surface-ring);
    background: var(--surface-strong);
    transition: border-color 140ms ease, box-shadow 140ms ease;
  }
  .field input:focus,
  .field select:focus {
    outline: none;
    border-color: color-mix(in srgb, var(--accent) 55%, transparent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }
  .field input:disabled,
  .field select:disabled {
    opacity: 0.6;
  }
  .date-wrap {
    position: relative;
    display: flex;
    align-items: stretch;
    min-width: 200px;
  }
  .date-wrap input[type='text'] {
    flex: 1;
    padding-right: 30px;
    min-width: 0;
  }
  .cal-icon {
    position: absolute;
    right: 8px;
    top: 50%;
    transform: translateY(-50%);
    display: flex;
    align-items: center;
    color: var(--text-muted);
    pointer-events: none;
    transition: color 140ms ease;
  }
  .date-err {
    font-size: 11px;
    color: var(--danger);
    margin-top: -2px;
  }
  .date-invalid input[type='text'] {
    border-color: var(--danger);
  }
  .date-invalid input[type='text']:focus {
    border-color: var(--danger);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--danger) 20%, transparent);
  }
  .inline {
    display: flex;
    gap: 6px;
  }
  .inline input[type='text'] {
    flex: 1;
  }
  .checkbox {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: var(--text-muted);
    margin: 10px 0;
    cursor: pointer;
  }
  .actions {
    display: flex;
    gap: 10px;
    align-items: center;
    flex-wrap: wrap;
    margin-top: 8px;
  }

  button.primary {
    background: var(--accent);
    color: white;
    border-color: transparent;
    box-shadow: 0 1px 0 rgba(0, 0, 0, 0.08), 0 6px 16px -8px var(--accent);
  }
  button.primary:hover:not(:disabled) {
    background: var(--accent-strong);
    border-color: transparent;
  }

  .pill {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 5px 10px;
    border-radius: 999px;
    font-size: 12.5px;
    line-height: 1.2;
  }
  .pill.ok {
    background: var(--success-soft);
    color: var(--success);
  }
  .pill.err {
    background: var(--danger-soft);
    color: var(--danger);
    white-space: pre-wrap;
  }

  .progress {
    margin-top: 14px;
    height: 8px;
    background: color-mix(in srgb, var(--text) 10%, transparent);
    border-radius: 999px;
    overflow: hidden;
  }
  .progress .bar {
    height: 100%;
    background: linear-gradient(90deg, var(--accent), var(--success));
    transition: width 160ms linear;
    border-radius: 999px;
  }

  .ok {
    color: var(--success);
  }
  .err {
    color: var(--danger);
    white-space: pre-wrap;
  }

  .scroll {
    max-height: 280px;
    overflow-y: auto;
    padding: 4px;
    margin: 10px 0 4px;
    list-style: none;
    border: 1px solid var(--surface-ring);
    border-radius: var(--radius-md);
    background: var(--surface-strong);
  }
  .scroll li {
    border-radius: var(--radius-sm);
  }
  .scroll li + li {
    margin-top: 2px;
  }
  .scroll li label {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 7px 10px;
    cursor: pointer;
    width: 100%;
    border-radius: var(--radius-sm);
    transition: background 120ms ease;
  }
  .scroll li label:hover {
    background: var(--accent-soft);
  }
  .name {
    font-weight: 500;
    flex: 1;
  }
  .meta {
    color: var(--text-subtle);
    font-size: 12.5px;
  }
  .summary {
    margin-top: 12px;
    padding: 12px 14px;
    border-radius: var(--radius-md);
    background: var(--success-soft);
    border: 1px solid color-mix(in srgb, var(--success) 25%, transparent);
  }
  .summary ul {
    margin: 6px 0 0;
    padding-left: 20px;
    color: var(--text);
  }

  section.danger {
    border: 1px solid var(--danger-ring);
    background:
      linear-gradient(180deg, var(--danger-soft), transparent 40%),
      var(--surface);
  }
  button.danger {
    background: var(--danger);
    color: white;
    border-color: transparent;
    box-shadow: 0 1px 0 rgba(0, 0, 0, 0.08), 0 6px 16px -8px var(--danger);
  }
  button.danger:hover:not(:disabled) {
    background: var(--danger-strong);
    border-color: transparent;
  }

  details {
    margin-top: 24px;
  }
  details summary {
    cursor: pointer;
    color: var(--text-subtle);
    padding: 6px 2px;
    user-select: none;
  }
  details summary:hover {
    color: var(--text-muted);
  }

  .preview-counting {
    margin: 4px 0 0;
    opacity: 0.6;
  }

  .preview-result {
    display: flex;
    align-items: baseline;
    gap: 6px;
    flex-wrap: wrap;
    margin-top: 6px;
  }

  .preview-count {
    font-size: 20px;
    font-weight: 600;
    letter-spacing: -0.02em;
    color: var(--text);
  }

  .preview-label {
    font-size: 13px;
    color: var(--text-muted);
  }

  .preview-label.not-on-mac-warn {
    color: var(--text-subtle);
  }

  .preview-sep {
    color: var(--text-subtle);
    font-size: 14px;
  }

  .pill-filter {
    background: var(--accent-soft);
    color: var(--accent);
    padding: 3px 8px;
    border-radius: 999px;
    font-size: 11.5px;
    font-weight: 500;
  }

  .pill-all {
    background: color-mix(in srgb, var(--text) 8%, transparent);
    color: var(--text-subtle);
    padding: 3px 8px;
    border-radius: 999px;
    font-size: 11.5px;
    font-weight: 500;
  }

  /* Orphan-clean modal */
  .modal-back {
    position: fixed;
    inset: 0;
    background: color-mix(in srgb, var(--bg) 55%, transparent);
    backdrop-filter: blur(6px);
    -webkit-backdrop-filter: blur(6px);
    display: grid;
    place-items: center;
    z-index: 100;
  }

  .modal-card {
    background: var(--surface);
    border: 1px solid var(--surface-ring);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-card), 0 24px 64px -16px rgba(0, 0, 0, 0.3);
    padding: 24px 28px;
    width: clamp(320px, 420px, 90vw);
    animation: card-in 260ms cubic-bezier(0.22, 1, 0.36, 1) both;
  }

  .modal-title {
    margin: 0 0 16px;
    font-size: 15px;
    font-weight: 600;
    letter-spacing: -0.01em;
  }

  .orphan-rows {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 12px;
  }

  .orphan-row {
    display: grid;
    grid-template-columns: 1fr auto auto;
    gap: 10px;
    align-items: baseline;
    padding: 8px 10px;
    border-radius: var(--radius-sm);
    background: var(--surface-strong);
    border: 1px solid var(--surface-ring);
  }

  .orphan-label {
    font-size: 13px;
    color: var(--text-muted);
  }

  .orphan-count {
    font-size: 13px;
    font-weight: 600;
    color: var(--text);
    text-align: right;
  }

  .orphan-size {
    font-size: 12px;
    color: var(--text-subtle);
    min-width: 56px;
    text-align: right;
  }

  .orphan-total {
    font-size: 13px;
    color: var(--text-muted);
    margin: 0 0 14px;
  }

  .icloud-notice {
    margin-top: 10px;
    padding: 12px 14px;
    border-radius: var(--radius-md);
    background: var(--accent-soft);
    border: 1px solid color-mix(in srgb, var(--accent) 20%, transparent);
  }
  .icloud-notice-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--text);
    margin-bottom: 4px;
  }
  .icloud-notice p {
    margin: 4px 0;
  }
</style>
