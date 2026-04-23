import { invoke } from '@tauri-apps/api/core';

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
  snapshotRoot?: string;
  deleteScope?: DeleteScope;
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
