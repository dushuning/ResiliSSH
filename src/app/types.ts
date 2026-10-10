// 前后端共享的数据结构（与 Rust 端 serde 结构对应）及界面内部类型。
export interface SshHostEntry {
  alias: string;
  hostname: string | null;
  user: string | null;
  port: number | null;
  identity_file: string | null;
  /** ProxyJump 中离目标最近的一跳：别名或 user@host:port。 */
  proxy_jump: string | null;
}


export interface ConnectionProfile {
  id: string;
  name: string;
  host: string;
  port: number;
  username: string;
  auth_kind: "password" | "private_key";
  key_path: string | null;
  password?: string | null;
  key_passphrase?: string | null;
  created_at: number;
  bookmarks: string[];
  post_commands: string[];
  remember_secrets: boolean;
  /** 经由的跳板机（另一台已保存服务器的 id）。 */
  jump_profile_id?: string | null;
}


export interface CommandOutcome {
  command: string;
  exit_code: number | null;
  cancelled: boolean;
  timed_out: boolean;
  output_tail: string[];
}


export interface CommandContext {
  remote_dir: string;
  file_name: string;
  remote_path: string;
  files: string[];
}


export interface ProfileSecrets {
  password: string | null;
  key_passphrase: string | null;
}


export interface UploadHistoryEntry {
  id: string;
  local_path: string;
  remote_path: string;
  host: string;
  port: number;
  username: string;
  file_size: number;
  status: string;
  retry_count: number;
  finished_at: number;
  message: string | null;
  direction?: string;
}


export interface HostKeyInfo {
  status: "trusted" | "unknown" | "changed";
  fingerprint: string;
  key_type: string;
}


export interface UploadCheckpoint {
  local_path: string;
  remote_path: string;
  host: string;
  port: number;
  username: string;
  auth_kind: "password" | "private_key";
  key_path: string | null;
  file_size: number;
  uploaded_bytes: number;
  verified_bytes?: number;
  retry_count?: number;
  failure_reason?: string | null;
  status: "in_progress" | "failed" | "completed";
}


export interface RemoteUploadProbe {
  resolved_remote_path: string;
  remote_exists: boolean;
  remote_size: number;
  local_size: number;
  verified_bytes: number;
  action:
    | "new"
    | "resume"
    | "overwrite"
    | "full_reupload"
    | "verify_retry"
    | "identical"
    | "insufficient_space";
  message: string;
}


export interface UploadProgressEvent {
  uploaded_bytes: number;
  total_bytes: number;
  status: string;
  message: string;
  retry_count: number;
  verify_summary?: string | null;
}


export interface UploadRetryEvent {
  delay_ms: number;
  message: string;
  retry_count: number;
}


export interface DownloadCheckpoint {
  local_path: string;
  remote_path: string;
  host: string;
  port: number;
  username: string;
  auth_kind: "password" | "private_key";
  key_path: string | null;
  file_size: number;
  downloaded_bytes: number;
  verified_bytes?: number;
  retry_count?: number;
  failure_reason?: string | null;
  status: "in_progress" | "failed" | "completed";
}


export interface RemoteDownloadProbe {
  resolved_local_path: string;
  resolved_remote_path: string;
  remote_exists: boolean;
  remote_size: number;
  local_size: number;
  verified_bytes: number;
  action:
    | "new"
    | "resume"
    | "overwrite"
    | "full_redownload"
    | "verify_retry"
    | "identical"
    | "insufficient_space";
  message: string;
}


export interface DownloadProgressEvent {
  downloaded_bytes: number;
  total_bytes: number;
  status: string;
  message: string;
  retry_count: number;
  verify_summary?: string | null;
}


export interface DownloadRetryEvent {
  delay_ms: number;
  message: string;
  retry_count: number;
}


export interface RemoteDirEntry {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
}


export interface RemoteDirListing {
  path: string;
  parent: string | null;
  entries: RemoteDirEntry[];
}


export type TransferDirection = "upload" | "download";


export type ThemePreference = "system" | "light" | "dark";


export type StatePanelKind = "info" | "warn" | "error" | "neutral";


export interface TransferStateView {
  kind: StatePanelKind;
  title: string;
  detail?: string;
  hint?: string;
  showClear: boolean;
}


export type PageId = "transfer" | "servers" | "history" | "logs" | "settings";

export type TransferView = "compose" | "run" | "result";

/** 展开后的待传文件。上传时 path 是本地路径，下载时是远端路径；rel_dir 是相对所选文件夹的目录。 */
export interface LocalEntry {
  path: string;
  rel_dir: string;
  size: number;
}


export interface ExpandedPaths {
  entries: LocalEntry[];
  truncated: boolean;
  /** 读不了的子目录：其中的文件不在 entries 里，必须提醒用户。 */
  unreadable_dirs: string[];
}


export type QueueItemStatus = "pending" | "running" | "done" | "failed" | "skipped" | "cancelled";


/**
 * 队列中的一个文件。source 是被传的文件（上传为本地路径、下载为远端路径），
 * target 是它要放进的目录（上传为远端目录、下载为本地目录），开始时分别填入两个路径输入框。
 */
export interface QueueItem {
  source: string;
  target: string;
  /** 展示用名称：文件夹上传时带上相对目录，便于区分同名文件。 */
  label: string;
  remote: string;
  /** 实际的本地文件路径（下载时由预检解析出），用于传输凭证。 */
  local: string;
  status: QueueItemStatus;
  message?: string;
  stats?: RunStats;
  /** 两端已完全一致而跳过，凭证里需注明「本次未传输」。 */
  identical?: boolean;
}


/** 单个文件传输的终态，由 handleProgressUpdate 交回给队列调度。 */
export interface RunStepResult {
  status: "completed" | "failed" | "cancelled";
  message: string;
  stats: RunStats;
}


export interface ConnectionReadiness {
  ready: boolean;
  summary: string;
  detail?: string;
}


/**
 * 预检、确认、启动一个上传。返回 false 表示用户在确认框里放弃了这个文件。
 * 单文件与队列共用：队列只是对每个文件依次调用它，续传、校验等行为完全一致。
 */
export type UploadStart = "started" | "declined" | "identical";


export type ResultKind = "ok" | "warn" | "error" | "paused";


export interface ReceiptFile {
  local: string;
  remote: string;
  skipped?: boolean;
}


export interface RunReceipt {
  direction: TransferDirection;
  endpoint: string;
  /** 经过的跳板机，按连接顺序从最外层写起。 */
  via: string[];
  startedAt: number;
  finishedAt: number;
  elapsedMs: number;
  retries: number;
  verification: string;
  files: ReceiptFile[];
}


export interface RunStats {
  elapsedMs: number;
  movedBytes: number;
  retries: number;
}

