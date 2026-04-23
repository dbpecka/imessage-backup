import { invoke } from '@tauri-apps/api/core';

/**
 * Tagged error payload emitted by the Rust backend. Mirrors the
 * `AppError` enum in src-tauri/src/error.rs (`#[serde(tag = "kind", content = "data")]`).
 */
export type AppError =
  | { kind: 'database'; data: string }
  | { kind: 'fullDiskAccess'; data: { path: string } }
  | { kind: 'io'; data: string }
  | { kind: 'other'; data: string };

export function isAppError(err: unknown): err is AppError {
  return (
    typeof err === 'object' &&
    err !== null &&
    'kind' in err &&
    typeof (err as { kind: unknown }).kind === 'string'
  );
}

export function isFdaError(err: unknown): boolean {
  return isAppError(err) && err.kind === 'fullDiskAccess';
}

/**
 * Human-friendly message for UI surfaces. Per-kind copy sits here so the
 * Rust side can stay terse and the UI can polish the edges without regex.
 */
export function appErrorMessage(err: unknown): string {
  if (!isAppError(err)) return String(err);
  switch (err.kind) {
    case 'database':
      // Sqlite errors arrive verbose ("SqliteFailure(Error { code: ... })").
      // Strip the wrapper the Rust crate normally adds so the message reads
      // cleanly in a toast.
      return `Database error: ${stripSqliteFailurePrefix(err.data)}`;
    case 'fullDiskAccess':
      return `Full Disk Access is required. Grant access to BubbleWrap in System Settings → Privacy & Security → Full Disk Access.`;
    case 'io':
      return `File system error: ${err.data}`;
    case 'other':
      return err.data;
  }
}

function stripSqliteFailurePrefix(msg: string): string {
  // rusqlite formats as "SqliteFailure(<code>) <message>" — keep the message.
  const match = msg.match(/^SqliteFailure\([^)]*\)\s*(.*)$/);
  return match ? match[1] : msg;
}

export type FdaStatus = {
  granted: boolean;
  path: string;
};

export type ProbeResult = {
  path: string;
  messageCount: number;
};

export type ChatSummary = {
  rowid: number;
  chatIdentifier: string;
  displayName: string | null;
  contactName: string | null;
  serviceName: string | null;
  participantCount: number;
  participantHandles: string[];
  messageCount: number;
};

export type ContactSummary = {
  rowid: number;
  id: string;
};

export type FilterSpec = {
  dateRange?: { start?: string; end?: string };
  chatIds?: number[];
  handleIds?: number[];
  attachments?: {
    types?: ('image' | 'video' | 'audio' | 'other')[];
    minBytes?: number;
    maxBytes?: number;
  };
};

export type BackupPreview = {
  messageCount: number;
  attachmentCount: number;
  attachmentBytes: number;
  onDiskCount: number;
  notOnMacCount: number;
  missingCount: number;
  hasFilters: boolean;
};

export type ExportFormat = 'html' | 'txt' | 'json' | 'pdf';

export type RunBackupArgs = {
  filter: FilterSpec;
  format: ExportFormat;
  destination: string;
  copyAttachments?: boolean;
};

export type BackupResult = {
  messageCount: number;
  attachmentCount: number;
  attachmentBytesCopied: number;
  conversationCount: number;
  manifestPath: string;
  exportPath: string;
  format: string;
};

export type ProgressPayload = {
  total: number;
  position: number;
  message: string;
  done: boolean;
};

export type ICloudState = 'enabled' | 'disabled' | 'unknown';

export type SafetyStatus = {
  messagesRunning: boolean;
  icloudMessages: ICloudState;
};

export type DeletePreview = {
  messageCount: number;
  attachmentCount: number;
  attachmentBytes: number;
  onDiskFileCount: number;
};

export type DeleteScope = 'both' | 'messages_only' | 'attachments_only';

export type RunDeleteArgs = {
  filter: FilterSpec;
  confirmationPhrase: string;
  backupVerified?: boolean;
  /** Required by the backend when backupVerified is false. */
  acknowledgeSkipBackup?: boolean;
  snapshotRoot?: string;
  deleteScope?: DeleteScope;
  /** Must be true when Messages in iCloud is enabled. Required by the backend. */
  acknowledgeIcloudSync?: boolean;
};

export type DeleteResult = {
  messagesDeleted: number;
  attachmentsDeleted: number;
  attachmentJoinsDeleted: number;
  chatMessageJoinsDeleted: number;
  orphanChatsDeleted: number;
  orphanHandlesDeleted: number;
  snapshotPath: string;
  onDiskFilesRemoved: number;
  onDiskFilesFailed: number;
  backupVerified: boolean;
};

export type OrphanScan = {
  dbOrphanCount: number;
  dbOrphanBytes: number;
  fsOrphanCount: number;
  fsOrphanBytes: number;
};

export type OrphanCleanResult = {
  dbRowsDeleted: number;
  dbFilesRemoved: number;
  dbFilesFailed: number;
  fsFilesRemoved: number;
  fsFilesFailed: number;
};


export const api = {
  checkFda: () => invoke<FdaStatus>('check_fda'),
  openFdaSettings: () => invoke<void>('open_fda_settings'),
  relaunchApp: () => invoke<void>('relaunch_app'),
  probeDb: () => invoke<ProbeResult>('probe_db'),
  listChats: () => invoke<ChatSummary[]>('list_chats'),
  listContacts: () => invoke<ContactSummary[]>('list_contacts'),
  previewBackup: (filter: FilterSpec) => invoke<BackupPreview>('preview_backup', { filter }),
  runBackup: (args: RunBackupArgs) => invoke<BackupResult>('run_backup', { args }),
  safetyStatus: () => invoke<SafetyStatus>('safety_status'),
  previewDelete: (filter: FilterSpec) => invoke<DeletePreview>('preview_delete', { filter }),
  runDelete: (args: RunDeleteArgs) => invoke<DeleteResult>('run_delete', { args }),
  scanOrphans: () => invoke<OrphanScan>('scan_orphans'),
  cleanOrphans: () => invoke<OrphanCleanResult>('clean_orphans'),
};
