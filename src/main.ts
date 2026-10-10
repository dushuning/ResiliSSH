import "./theme.css";
import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { ActivityLogView, copyText, type ActivityEntry, type LogFilter } from "./ui/activityLog";
import { BlockMap, type BlockMapPhase } from "./ui/blockMap";
import { icons } from "./ui/icons";
import { Popover, type PopoverItem } from "./ui/popover";
import { appTemplate } from "./ui/template";
import { language, languagePreference, locale, setLanguagePreference, t, type LanguagePreference } from "./i18n";

interface SshHostEntry {
  alias: string;
  hostname: string | null;
  user: string | null;
  port: number | null;
  identity_file: string | null;
}

interface ConnectionProfile {
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
}

interface CommandOutcome {
  command: string;
  exit_code: number | null;
  cancelled: boolean;
  timed_out: boolean;
  output_tail: string[];
}

interface CommandContext {
  remote_dir: string;
  file_name: string;
  remote_path: string;
  files: string[];
}

interface ProfileSecrets {
  password: string | null;
  key_passphrase: string | null;
}

interface UploadHistoryEntry {
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

interface HostKeyInfo {
  status: "trusted" | "unknown" | "changed";
  fingerprint: string;
  key_type: string;
}

interface UploadCheckpoint {
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

interface RemoteUploadProbe {
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

interface UploadProgressEvent {
  uploaded_bytes: number;
  total_bytes: number;
  status: string;
  message: string;
  retry_count: number;
  verify_summary?: string | null;
}

interface UploadRetryEvent {
  delay_ms: number;
  message: string;
  retry_count: number;
}

interface DownloadCheckpoint {
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

interface RemoteDownloadProbe {
  resolved_local_path: string;
  resolved_remote_path: string;
  remote_exists: boolean;
  remote_size: number;
  local_size: number;
  verified_bytes: number;
  action: "new" | "resume" | "overwrite" | "full_redownload" | "verify_retry";
  message: string;
}

interface DownloadProgressEvent {
  downloaded_bytes: number;
  total_bytes: number;
  status: string;
  message: string;
  retry_count: number;
  verify_summary?: string | null;
}

interface DownloadRetryEvent {
  delay_ms: number;
  message: string;
  retry_count: number;
}

interface RemoteDirEntry {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
}

interface RemoteDirListing {
  path: string;
  parent: string | null;
  entries: RemoteDirEntry[];
}

type TransferDirection = "upload" | "download";

const THEME_STORAGE_KEY = "resiliss-theme";
type ThemePreference = "system" | "light" | "dark";

const systemDarkQuery = window.matchMedia("(prefers-color-scheme: dark)");

function readThemePreference(): ThemePreference {
  const saved = localStorage.getItem(THEME_STORAGE_KEY);
  return saved === "light" || saved === "dark" ? saved : "system";
}

/** 「跟随系统」在这里解析成实际明暗写到 data-theme，CSS 只需维护一份深色变量。 */
function resolveTheme(mode: ThemePreference): "light" | "dark" {
  if (mode !== "system") return mode;
  return systemDarkQuery.matches ? "dark" : "light";
}

// 在渲染前同步应用主题，避免启动时先闪一下浅色。
document.documentElement.dataset.theme = resolveTheme(readThemePreference());

const APP_NAME = "ResiliSSH";

type StatePanelKind = "info" | "warn" | "error" | "neutral";

interface TransferStateView {
  kind: StatePanelKind;
  title: string;
  detail?: string;
  hint?: string;
  showClear: boolean;
}

const app = document.querySelector<HTMLDivElement>("#app")!;

// 先告诉后端界面语言：之后后端生成的错误信息、活动日志都按这个语言输出。
void invoke("set_language", { language: language() }).catch(() => {
  // 浏览器预览模式下没有后端。
});

app.innerHTML = appTemplate(APP_NAME);

const profileSelect = document.querySelector<HTMLSelectElement>("#profileSelect")!;
const profileNameInput = document.querySelector<HTMLInputElement>("#profileName")!;
const connectionSummary = document.querySelector<HTMLSpanElement>("#connectionSummary")!;
const connectionStatus = document.querySelector<HTMLDivElement>("#connectionStatus")!;
const authHint = document.querySelector<HTMLParagraphElement>("#authHint")!;
const rememberSecretsInput = document.querySelector<HTMLInputElement>("#rememberSecrets")!;
const rememberRow = document.querySelector<HTMLLabelElement>("#rememberRow")!;
const saveProfileBtn = document.querySelector<HTMLButtonElement>("#saveProfileBtn")!;
const deleteProfileBtn = document.querySelector<HTMLButtonElement>("#deleteProfileBtn")!;
const hostSelect = document.querySelector<HTMLSelectElement>("#hostSelect")!;
const hostInput = document.querySelector<HTMLInputElement>("#host")!;
const portInput = document.querySelector<HTMLInputElement>("#port")!;
const usernameInput = document.querySelector<HTMLInputElement>("#username")!;
const authTypeSelect = document.querySelector<HTMLSelectElement>("#authType")!;
const keyFields = document.querySelector<HTMLDivElement>("#keyFields")!;
const passwordFields = document.querySelector<HTMLDivElement>("#passwordFields")!;
const keyPathInput = document.querySelector<HTMLInputElement>("#keyPath")!;
const keyPassphraseInput = document.querySelector<HTMLInputElement>("#keyPassphrase")!;
const passwordInput = document.querySelector<HTMLInputElement>("#password")!;
const testConnBtn = document.querySelector<HTMLButtonElement>("#testConnBtn")!;
const transferStatePanel = document.querySelector<HTMLDivElement>("#transferStatePanel")!;
const statePanelTitle = document.querySelector<HTMLDivElement>("#statePanelTitle")!;
const statePanelDetail = document.querySelector<HTMLDivElement>("#statePanelDetail")!;
const statePanelHint = document.querySelector<HTMLDivElement>("#statePanelHint")!;
const localPathLabel = document.querySelector<HTMLLabelElement>("#localPathLabel")!;
const remotePathLabel = document.querySelector<HTMLLabelElement>("#remotePathLabel")!;
const fileChip = document.querySelector<HTMLDivElement>("#fileChip")!;
const fileChipName = document.querySelector<HTMLDivElement>("#fileChipName")!;
const fileChipPath = document.querySelector<HTMLDivElement>("#fileChipPath")!;
const changeFileBtn = document.querySelector<HTMLButtonElement>("#changeFileBtn")!;
const dropHint = document.querySelector<HTMLParagraphElement>("#dropHint")!;
const pathHint = document.querySelector<HTMLParagraphElement>("#pathHint")!;
const forceOverwriteLabel = document.querySelector<HTMLSpanElement>("#forceOverwriteLabel")!;
const localPathInput = document.querySelector<HTMLInputElement>("#localPath")!;
const remotePathInput = document.querySelector<HTMLInputElement>("#remotePath")!;
const browseRemoteBtn = document.querySelector<HTMLButtonElement>("#browseRemoteBtn")!;
const resolvedPathHint = document.querySelector<HTMLParagraphElement>("#resolvedPathHint")!;
const dropZone = document.querySelector<HTMLDivElement>("#dropZone")!;
const forceOverwriteInput = document.querySelector<HTMLInputElement>("#forceOverwrite")!;
const modeHint = document.querySelector<HTMLParagraphElement>("#modeHint")!;
const transferBtn = document.querySelector<HTMLButtonElement>("#transferBtn")!;
const cancelBtn = document.querySelector<HTMLButtonElement>("#cancelBtn")!;
const clearCheckpointBtn = document.querySelector<HTMLButtonElement>("#clearCheckpointBtn")!;
const progressWrap = document.querySelector<HTMLDivElement>("#progressWrap")!;
const progressBadge = document.querySelector<HTMLSpanElement>("#progressBadge")!;
const progressDetail = document.querySelector<HTMLDivElement>("#progressDetail")!;
const progressSpeed = document.querySelector<HTMLDivElement>("#progressSpeed")!;
const verifySummary = document.querySelector<HTMLDivElement>("#verifySummary")!;
const statusText = document.querySelector<HTMLDivElement>("#statusText")!;
const progressPercent = document.querySelector<HTMLDivElement>("#progressPercent")!;
const runSummary = document.querySelector<HTMLDivElement>("#runSummary")!;
const mapCaption = document.querySelector<HTMLSpanElement>("#mapCaption")!;
const composeView = document.querySelector<HTMLDivElement>("#composeView")!;
const runView = document.querySelector<HTMLDivElement>("#runView")!;
const resultView = document.querySelector<HTMLDivElement>("#resultView")!;
const resultCard = document.querySelector<HTMLDivElement>("#resultCard")!;
const resultIcon = document.querySelector<HTMLDivElement>("#resultIcon")!;
const resultTitle = document.querySelector<HTMLDivElement>("#resultTitle")!;
const resultSubtitle = document.querySelector<HTMLDivElement>("#resultSubtitle")!;
const resultStats = document.querySelector<HTMLDListElement>("#resultStats")!;
const resultBackBtn = document.querySelector<HTMLButtonElement>("#resultBackBtn")!;
const editServerBtn = document.querySelector<HTMLButtonElement>("#editServerBtn")!;
const newServerBtn = document.querySelector<HTMLButtonElement>("#newServerBtn")!;
const serverList = document.querySelector<HTMLDivElement>("#serverList")!;
const pendingSection = document.querySelector<HTMLElement>("#pendingSection")!;
const pendingList = document.querySelector<HTMLDivElement>("#pendingList")!;
const historyList = document.querySelector<HTMLDivElement>("#historyList")!;
const navTransferLive = document.querySelector<HTMLSpanElement>("#navTransferLive")!;
const navServerCount = document.querySelector<HTMLSpanElement>("#navServerCount")!;
const navHistoryBadge = document.querySelector<HTMLSpanElement>("#navHistoryBadge")!;
const miniTransfer = document.querySelector<HTMLButtonElement>("#miniTransfer")!;
const miniTransferLabel = document.querySelector<HTMLSpanElement>("#miniTransferLabel")!;
const miniTransferPct = document.querySelector<HTMLSpanElement>("#miniTransferPct")!;
const miniTransferFill = document.querySelector<HTMLDivElement>("#miniTransferFill")!;
const miniTransferSpeed = document.querySelector<HTMLDivElement>("#miniTransferSpeed")!;
const appVersion = document.querySelector<HTMLSpanElement>("#appVersion")!;
const viewLogBtn = document.querySelector<HTMLButtonElement>("#viewLogBtn")!;
const bookmarkBtn = document.querySelector<HTMLButtonElement>("#bookmarkBtn")!;
const pathMenuBtn = document.querySelector<HTMLButtonElement>("#pathMenuBtn")!;
const pathMenu = new Popover(
  document.querySelector<HTMLDivElement>("#pathMenu")!,
  document.querySelector<HTMLInputElement>("#remotePath")!.closest<HTMLElement>(".path-field")!,
);
const queueChip = document.querySelector<HTMLSpanElement>("#queueChip")!;
const resultFiles = document.querySelector<HTMLUListElement>("#resultFiles")!;
const postCommandInput = document.querySelector<HTMLInputElement>("#postCommand")!;
const saveCommandBtn = document.querySelector<HTMLButtonElement>("#saveCommandBtn")!;
const commandMenuBtn = document.querySelector<HTMLButtonElement>("#commandMenuBtn")!;
const commandMenu = new Popover(
  document.querySelector<HTMLDivElement>("#commandMenu")!,
  document.querySelector<HTMLDivElement>("#commandField")!,
);
const commandConsole = document.querySelector<HTMLDivElement>("#commandConsole")!;
const commandConsoleCmd = document.querySelector<HTMLSpanElement>("#commandConsoleCmd")!;
const commandOutput = document.querySelector<HTMLPreElement>("#commandOutput")!;
const resultCommand = document.querySelector<HTMLDivElement>("#resultCommand")!;
const resultCommandLine = document.querySelector<HTMLDivElement>("#resultCommandLine")!;
const resultCommandOutput = document.querySelector<HTMLPreElement>("#resultCommandOutput")!;
const legendResumed = document.querySelector<HTMLSpanElement>("#legendResumed")!;
const legendRetry = document.querySelector<HTMLSpanElement>("#legendRetry")!;
const mapLegend = document.querySelector<HTMLDivElement>("#mapLegend")!;
const legendDoneLabel = document.querySelector<HTMLSpanElement>("#legendDoneLabel")!;
const activityLog = new ActivityLogView(
  document.querySelector<HTMLDivElement>("#logList")!,
  document.querySelector<HTMLButtonElement>("#logJumpBtn")!,
  document.querySelector<HTMLSpanElement>("#navLogBadge")!,
);
const confirmOverlay = document.querySelector<HTMLDivElement>("#confirmOverlay")!;
const confirmTitle = document.querySelector<HTMLDivElement>("#confirmTitle")!;
const confirmBody = document.querySelector<HTMLDivElement>("#confirmBody")!;
const confirmCancel = document.querySelector<HTMLButtonElement>("#confirmCancel")!;
const confirmOk = document.querySelector<HTMLButtonElement>("#confirmOk")!;
const remoteBrowseOverlay = document.querySelector<HTMLDivElement>("#remoteBrowseOverlay")!;
const remoteBrowseTitle = document.querySelector<HTMLDivElement>("#remoteBrowseTitle")!;
const remoteBrowsePath = document.querySelector<HTMLInputElement>("#remoteBrowsePath")!;
const remoteBrowseGoBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseGoBtn")!;
const remoteBrowseStatus = document.querySelector<HTMLDivElement>("#remoteBrowseStatus")!;
const remoteBrowseHint = document.querySelector<HTMLParagraphElement>("#remoteBrowseHint")!;
const remoteBrowseList = document.querySelector<HTMLDivElement>("#remoteBrowseList")!;
const remoteBrowseUpBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseUpBtn")!;
const remoteBrowseSelectDirBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseSelectDirBtn")!;
const remoteBrowseNewDirBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseNewDirBtn")!;
const remoteBrowseNewDirRow = document.querySelector<HTMLDivElement>("#remoteBrowseNewDirRow")!;
const remoteBrowseNewDirName = document.querySelector<HTMLInputElement>("#remoteBrowseNewDirName")!;
const remoteBrowseCreateDirBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseCreateDirBtn")!;
const remoteBrowseCancelNewDirBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseCancelNewDirBtn")!;
const remoteBrowseCloseBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseCloseBtn")!;

let isTransferring = false;
let cancelInFlight = false;
let isReconnecting = false;
/** 传输已发起但尚未收到首条进度：与 stall 等待区分，避免误判为卡死 */
let isConnecting = false;
/** 块读回校验进行中：对用户透明，仅用于抑制误报 stall */
let isChunkVerifying = false;
/** 全文件 SHA-256 校验进行中 */
let isFullVerifying = false;
let retryDelayMs = 0;
let retryScheduledAt = 0;
let transferDirection: TransferDirection = "upload";
let retryCount = 0;
let historyEntries: UploadHistoryEntry[] = [];
let savedProfiles: ConnectionProfile[] = [];
let activeUploadCheckpoint: UploadCheckpoint | null = null;
let activeDownloadCheckpoint: DownloadCheckpoint | null = null;
let lastProbe: RemoteUploadProbe | RemoteDownloadProbe | null = null;
let lastProgressEventAt = 0;
let speedSamples: { t: number; bytes: number }[] = [];
let speedDisplayTimer: number | null = null;
let progressWatchdogTimer: number | null = null;
const UPLOAD_CHUNK_BYTES = 1024 * 1024;
const SPEED_SAMPLE_WINDOW_MS = 3000;
const SPEED_REFRESH_MS = 1000;
const SPEED_STALE_MS = 8_000;
const UI_STALL_WARN_MS = 10_000;
const CONFIRM_ANIM_MS = 200;
/** 路径预检仅在失焦或程序化改路径时触发，避免每次按键都连 SSH */
const PROBE_DEBOUNCE_MS = 400;

type PageId = "transfer" | "servers" | "history" | "logs" | "settings";
type TransferView = "compose" | "run" | "result";
const PAGE_ORDER: readonly PageId[] = ["transfer", "servers", "history", "logs", "settings"];
let currentPage: PageId = "transfer";
let currentView: TransferView = "compose";

/** 当前这次传输的总字节与起止信息：结果页的耗时、平均速度、本次实传量都从这里算。 */
let progressTotalBytes = 0;
let progressTransferredBytes = 0;
let runStartedAt = 0;
let runStartBytes: number | null = null;
/** 传输开始时锁定方向：取消、结果展示都以它为准，不受之后界面状态影响。 */
let runDirection: TransferDirection = "upload";
/** 本次传输的远端完整路径（预检解析后的结果），传完后命令的变量从这里取。 */
let runRemotePath = "";
/** 传输开始时锁定的「传完后执行」命令：传输中输入框已锁定，这里只是避免读到之后的改动。 */
let runPostCommandTemplate = "";
/** 传完后命令执行中：此时传输已结束（isTransferring=false），但界面仍停留在进行中视图。 */
let commandRunning = false;

/**
 * 待上传的本地文件。单文件时与 localPathInput 一致；多选/多拖入时其余文件也在这里。
 * localPathInput 始终保存第一个文件，原有的单文件逻辑（预检、路径预览）无需感知队列。
 */
let selectedUploads: string[] = [];

type QueueItemStatus = "pending" | "running" | "done" | "failed" | "skipped" | "cancelled";

interface QueueItem {
  local: string;
  remote: string;
  status: QueueItemStatus;
  message?: string;
  stats?: RunStats;
}

/** 单个文件传输的终态，由 handleProgressUpdate 交回给队列调度。 */
interface RunStepResult {
  status: "completed" | "failed" | "cancelled";
  message: string;
  stats: RunStats;
}

/** 队列执行中的状态；为 null 表示当前是普通单文件传输。 */
let activeQueue: { items: QueueItem[]; index: number } | null = null;
let resolveQueueStep: ((result: RunStepResult) => void) | null = null;

const blockMap = new BlockMap(
  document.querySelector<HTMLCanvasElement>("#blockMap")!,
  UPLOAD_CHUNK_BYTES,
);

const TRANSFER_LOCK_SELECTORS = [
  "#profileSelect",
  "#profileName",
  "#saveProfileBtn",
  "#deleteProfileBtn",
  "#hostSelect",
  "#host",
  "#port",
  "#username",
  "#authType",
  "#keyPath",
  "#keyPassphrase",
  "#password",
  "#pickKeyBtn",
  "#localPath",
  "#remotePath",
  "#browseRemoteBtn",
  "#pickFileBtn",
  "#changeFileBtn",
  "#forceOverwrite",
  'input[name="transferDirection"]',
  'input[name="transferMode"]',
  "#newServerBtn",
  ".server-item",
  "#rememberSecrets",
  "#bookmarkBtn",
  "#pathMenuBtn",
  "#postCommand",
  "#saveCommandBtn",
  "#commandMenuBtn",
] as const;

let probeDebounceTimer: number | null = null;
let backgroundProbeGeneration = 0;

function isDownloadMode(): boolean {
  return transferDirection === "download";
}

function getActiveCheckpoint(): UploadCheckpoint | DownloadCheckpoint | null {
  return isDownloadMode() ? activeDownloadCheckpoint : activeUploadCheckpoint;
}

function getTransferredBytes(cp: UploadCheckpoint | DownloadCheckpoint): number {
  if ("uploaded_bytes" in cp) return cp.uploaded_bytes;
  return cp.downloaded_bytes;
}

function setTransferFormLocked(locked: boolean) {
  // 必须用 querySelectorAll：单选组有多个同名 input，只锁第一个会让用户在传输中
  // 切换上传/下载方向，之后「取消」会发给错误方向的任务。
  for (const selector of TRANSFER_LOCK_SELECTORS) {
    document
      .querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLButtonElement>(selector)
      .forEach((el) => {
        el.disabled = locked;
      });
  }
  localPathInput.readOnly = locked;
  remotePathInput.readOnly = locked;
}

function setTransferring(running: boolean) {
  isTransferring = running;
  transferBtn.disabled = running;
  testConnBtn.disabled = running;
  setTransferFormLocked(running);
  syncDirectionLock();
  cancelBtn.textContent = t("取消传输");
  cancelBtn.classList.remove("is-cancelling");
  if (running) {
    isConnecting = true;
    runDirection = transferDirection;
    clearProbeDebounce();
    backgroundProbeGeneration++;
    lastProbe = null;
    hideVerifySummary();
    statusText.textContent = "";
    beginRunVisuals();
    showTransferView("run");
    startProgressWatchdog();
  } else {
    cancelInFlight = false;
    cancelBtn.disabled = false;
    isReconnecting = false;
    isConnecting = false;
    isChunkVerifying = false;
    isFullVerifying = false;
    retryDelayMs = 0;
    retryScheduledAt = 0;
    stopProgressWatchdog();
    progressDetail.classList.add("hidden");
    hideProgressSpeed();
    statusText.classList.remove("hidden");
  }
  updateTransferStatePanel();
  refreshLiveVisuals();
}

/** 传输页三种视图互斥：编辑 → 进行中 → 结果；页脚按钮随视图切换，主操作位置保持不变。 */
function showTransferView(view: TransferView) {
  currentView = view;
  composeView.classList.toggle("hidden", view !== "compose");
  runView.classList.toggle("hidden", view !== "run");
  resultView.classList.toggle("hidden", view !== "result");
  transferBtn.classList.toggle("hidden", view === "run");
  cancelBtn.classList.toggle("hidden", view !== "run");
  resultBackBtn.classList.toggle("hidden", view !== "result");
  // 传输中和出结果后，用户最想知道「刚才到底发生了什么」，此时才露出日志入口。
  viewLogBtn.classList.toggle("hidden", view === "compose");
  syncDirectionLock();
  updateTransferButtonLabel();
}

/** 方向只能在编辑视图里改：进行中/结果视图展示的是某个方向的具体任务，切换会让界面与任务对不上。 */
function syncDirectionLock() {
  document
    .querySelectorAll<HTMLInputElement>('input[name="transferDirection"]')
    .forEach((el) => {
      el.disabled = currentView !== "compose";
    });
}

function updateTransferButtonLabel() {
  const verb = isDownloadMode() ? t("下载") : t("上传");
  // 成功后唯一的下一步是「再传一个」，把它升级为主按钮；其他情况它只是次要的「返回修改」。
  // 「传完但命令失败」也算传输成功：下一步同样是「再传一个」，重试传输没有意义。
  const successResult = currentView === "result" && resultCard.dataset.transferOk === "1";
  resultBackBtn.classList.toggle("btn-primary", successResult);
  resultBackBtn.classList.toggle("btn-large", successResult);
  resultBackBtn.classList.toggle("btn-secondary", !successResult);
  if (successResult) {
    transferBtn.classList.add("hidden");
    resultBackBtn.textContent = t("再{verb}一个", { verb });
    return;
  }
  resultBackBtn.textContent = t("返回修改");
  if (currentView === "result") {
    const retryLabel = isQueueSelection() ? t("重试 {length} 个文件", { length: selectedUploads.length }) : t("重试");
    transferBtn.innerHTML = resultCard.classList.contains("paused") ? t("继续传输") : retryLabel;
  } else {
    // 用整句翻译而不是拼接动词：后端提示里会引用按钮原文（如 click "Start upload"），两边必须一致。
    transferBtn.innerHTML = `${isDownloadMode() ? t("开始下载") : t("开始上传")} <kbd>⌘↩</kbd>`;
  }
}

function fileNameOf(path: string): string {
  return path.split(/[/\\]/).filter(Boolean).pop() ?? path;
}

/** 进入进行中视图前重置分块地图，并写好「谁 → 哪里」的摘要行。 */
function beginRunVisuals() {
  progressTotalBytes = 0;
  progressTransferredBytes = 0;
  runStartedAt = Date.now();
  runStartBytes = null;
  blockMap.reset();
  blockMap.setVerifiedPerBlock(isReliableMode());
  if (activeQueue) {
    queueChip.textContent = t("第 {p1} / {length} 个", { p1: activeQueue.index + 1, length: activeQueue.items.length });
    queueChip.classList.remove("hidden");
  } else {
    queueChip.classList.add("hidden");
    renderQueueResultFiles(null);
  }
  syncMapLegend();
  mapCaption.textContent = "";
  progressPercent.textContent = "0%";

  const download = runDirection === "download";
  const local = localPathInput.value.trim();
  const remote = remotePathInput.value.trim();
  const host = hostInput.value.trim();
  const name = document.createElement("strong");
  name.textContent = download ? fileNameOf(remote) : fileNameOf(local);
  const target = document.createElement("span");
  target.className = "run-target";
  target.textContent = download ? local : `${host}:${remote}`;
  target.title = target.textContent;
  const arrow = document.createElement("span");
  arrow.innerHTML = icons.arrowRight;
  runSummary.replaceChildren(name, arrow, target);
}

function updateTransferDirectionUI() {
  const download = isDownloadMode();
  localPathLabel.textContent = download ? t("保存到") : t("本地文件");
  remotePathLabel.textContent = download ? t("远端文件") : t("远端路径");
  localPathInput.placeholder = download
    ? t("本地目录（以 / 结尾）或完整文件路径")
    : t("选择或拖拽文件到此处");
  remotePathInput.placeholder = download
    ? "/home/user/file.jar"
    : t("/home/user/dir/ 或 /home/user/file.jar");
  pathHint.textContent = download
    ? t("保存路径填目录时自动拼接远端文件名")
    : t("以 / 结尾表示目录，会自动拼接文件名");
  dropHint.textContent = download
    ? t("点「选择…」指定保存目录")
    : t("也可以直接把文件拖到窗口里");
  dropZone.classList.toggle("download-mode", download);
  forceOverwriteLabel.textContent = t("强制从头覆盖");
  forceOverwriteInput.parentElement!.title = download
    ? t("忽略断点与本地已有文件，从头重新下载")
    : t("忽略断点与远端已有数据，从头重新上传");
  updateTransferButtonLabel();
  browseRemoteBtn.textContent = t("浏览…");
  updateRemoteBrowseModeUi();
  updateResolvedPathHint();
  updateTransferStatePanel();
}

function isReliableMode(): boolean {
  const selected = document.querySelector<HTMLInputElement>(
    'input[name="transferMode"]:checked',
  );
  return selected?.value !== "fast";
}

function updateModeHint() {
  modeHint.textContent = isReliableMode()
    ? t("每块写完读回比对，略慢但最稳")
    : t("只在传完后整体校验，速度更快");
}

interface ConnectionReadiness {
  ready: boolean;
  summary: string;
  detail?: string;
}

function getConnectionReadiness(): ConnectionReadiness {
  const host = hostInput.value.trim();
  const user = usernameInput.value.trim();
  const port = Number(portInput.value) || 22;
  const profileSelected = profileSelect.value !== "";
  const profileLabel = profileSelected
    ? (profileSelect.options[profileSelect.selectedIndex]?.textContent ?? "").trim()
    : "";

  if (!host && !user) {
    return { ready: false, summary: t("未配置连接") };
  }

  const endpoint = user && host ? `${user}@${host}:${port}` : host || user;
  const issues: string[] = [];

  if (!host) issues.push(t("缺主机"));
  if (!user) issues.push(t("缺用户名"));

  const needsPassword = authTypeSelect.value === "password";
  const missingPassword = needsPassword && !passwordInput.value;
  const missingKey = !needsPassword && !keyPathInput.value.trim();

  // 已保存的服务器密码在钥匙串里、连接时才读取，此时输入框为空不代表缺密码。
  if (missingPassword && !profileRemembersSecrets()) issues.push(t("缺密码"));
  if (missingKey) issues.push(t("缺私钥"));
  if (!profileSelected && (missingPassword || missingKey)) {
    issues.unshift(t("未选连接"));
  }

  if (issues.length === 0) {
    // 选择器本身已显示服务器名称，摘要只补充地址；未保存的连接才需要在摘要里说明。
    const summary = profileLabel ? endpoint : t("{endpoint} · 未保存", { endpoint });
    return { ready: true, summary };
  }

  return {
    ready: false,
    summary: endpoint,
    detail: issues.join(" · "),
  };
}

function updateConnectionSummary() {
  const state = getConnectionReadiness();
  let summaryText: string;
  if (state.summary === t("未配置连接")) {
    summaryText = t("还没有配置服务器，点「编辑」添加");
  } else if (state.detail) {
    summaryText = `${state.summary} · ${state.detail}`;
  } else {
    summaryText = state.summary;
  }
  connectionSummary.textContent = summaryText;
  connectionSummary.title = summaryText;
  connectionSummary.classList.toggle("ok", state.ready);
  connectionSummary.classList.toggle("warn", !state.ready);
  updateAuthHintVisibility();
  highlightSelectedServer();
  updateBookmarkButton();
}

/** 「记住密码」只在需要输入秘密（密码登录或填了私钥口令）时显示，私钥免口令时不打扰用户。 */
function updateAuthHintVisibility() {
  const needsSecret = authTypeSelect.value === "password" || Boolean(keyPassphraseInput.value);
  rememberRow.classList.toggle("hidden", !needsSecret);
  authHint.classList.toggle("hidden", !needsSecret);
  authHint.textContent = rememberSecretsInput.checked
    ? t("密码与私钥口令保存在 macOS 钥匙串中，不会写入配置文件")
    : t("不保存密码：只在本次运行中使用，下次选择该服务器时需要重新输入");
}

/**
 * 不记住密码的服务器，本次运行中输入过的密码/口令暂存在内存里：切到别的服务器再切回来不必重输，
 * 退出应用即丢失，正是「仅本次使用」的含义。
 */
const sessionSecrets = new Map<string, { password: string; passphrase: string }>();

function rememberForSession() {
  const id = profileSelect.value;
  if (!id || profileRemembersSecrets()) return;
  sessionSecrets.set(id, { password: passwordInput.value, passphrase: keyPassphraseInput.value });
}

/** 选中的已保存服务器是否把密码存在钥匙串里；未保存的连接视为否。 */
function profileRemembersSecrets(): boolean {
  return currentProfile()?.remember_secrets ?? false;
}

function validateConnectionBeforeTransfer(): void {
  const state = getConnectionReadiness();
  if (state.ready) return;

  const host = hostInput.value.trim();
  const user = usernameInput.value.trim();
  const profileSelected = profileSelect.value !== "";

  if (!host || !user) {
    throw new Error(
      t("还没有选择服务器。请在上方选择已保存的服务器，或到「服务器」页添加"),
    );
  }

  if (authTypeSelect.value === "password" && !passwordInput.value) {
    throw new Error(
      profileSelected
        ? t("所选服务器没有保存密码。请到「服务器」页填写密码并保存")
        : t("密码为空。请到「服务器」页填写密码"),
    );
  }

  if (authTypeSelect.value === "key" && !keyPathInput.value.trim()) {
    throw new Error(t("请到「服务器」页选择私钥文件"));
  }

  throw new Error(
    state.detail
      ? t("连接未就绪（{detail}）。请到「服务器」页检查", { detail: state.detail })
      : t("连接未就绪，请到「服务器」页检查"),
  );
}

function analyzeCheckpointState(
  cp: UploadCheckpoint | DownloadCheckpoint,
  download: boolean,
): TransferStateView {
  const transferred = getTransferredBytes(cp);
  const total = cp.file_size;
  const pct = total > 0 ? Math.round((transferred / total) * 100) : 0;
  const action = download ? t("开始下载") : t("开始上传");
  const verifyReadFailed = cp.failure_reason === "verify_read_failed";
  const needsRedo =
    cp.failure_reason === "hash_mismatch" ||
    (cp.status === "failed" && transferred >= total && !verifyReadFailed);

  if (verifyReadFailed && transferred >= total) {
    return {
      kind: "info",
      title: t("传输已完成，待最终校验"),
      detail: t("各数据块均已通过校验（{p1}）", { p1: formatBytes(total) }),
      hint: t("直接点「{action}」重试校验即可，无需从头重传", { action }),
      showClear: true,
    };
  }
  if (needsRedo) {
    return {
      kind: "error",
      title: download ? t("校验未通过，需重新下载") : t("校验未通过，需重新上传"),
      detail: t("已传输 {p1}，但完整性校验失败", { p1: formatBytes(transferred) }),
      hint: t("请勾选「强制覆盖」或先清除断点后再试"),
      showClear: true,
    };
  }
  if (transferred > 0 && transferred < total) {
    return {
      kind: "info",
      title: t("可续传（{pct}%）", { pct }),
      detail: t("{p1} {p2} / {p3}", { p1: download ? t("已下") : t("已传"), p2: formatBytes(transferred), p3: formatBytes(total) }),
      hint: t("点击「{action}」将从断点继续", { action }),
      showClear: true,
    };
  }
  if (transferred === 0 && cp.status === "failed") {
    return {
      kind: "warn",
      title: download ? t("上次下载未完成") : t("上次上传未完成"),
      detail: t("断点记录无有效进度"),
      hint: t("点击「{action}」将重新传输；若远端/本地已有文件会自动处理", { action }),
      showClear: true,
    };
  }
  return {
    kind: "warn",
    title: download ? t("检测到未完成下载") : t("检测到未完成上传"),
    detail: transferred > 0 ? `${formatBytes(transferred)} / ${formatBytes(total)}` : undefined,
    hint: t("点击「{action}」继续", { action }),
    showClear: true,
  };
}

function analyzeProbeState(
  probe: RemoteUploadProbe | RemoteDownloadProbe,
  download: boolean,
): TransferStateView {
  if (forceOverwriteInput.checked) {
    return {
      kind: "warn",
      title: download ? t("将强制覆盖本地文件") : t("将强制覆盖远端文件"),
      detail: probe.message,
      hint: t("开始传输前会删除已有文件并从头重来"),
      showClear: false,
    };
  }

  const action = probe.action;
  if (action === "identical") {
    return {
      kind: "info",
      title: t("远端已是最新"),
      detail: probe.message,
      hint: t("开始后将直接完成，不会重新上传；如需重传请勾选「强制从头覆盖」"),
      showClear: false,
    };
  }
  if (action === "insufficient_space") {
    return {
      kind: "error",
      title: t("远端磁盘空间不足"),
      detail: probe.message,
      hint: t("请清理服务器空间或换一个目录后再试"),
      showClear: false,
    };
  }
  if (action === "verify_retry") {
    return {
      kind: "info",
      title: t("传输已完成，待最终校验"),
      detail: probe.message,
      hint: download ? t("直接开始下载即可重试校验") : t("直接开始上传即可重试校验"),
      showClear: false,
    };
  }
  if (action === "full_reupload" || action === "full_redownload") {
    return {
      kind: "error",
      title: action === "full_redownload" ? t("需从头重下") : t("需从头重传"),
      detail: probe.message,
      hint: t("继续将删除已有文件并重新传输"),
      showClear: false,
    };
  }
  if (action === "overwrite") {
    return {
      kind: "warn",
      title: download ? t("将覆盖本地文件") : t("将覆盖远端文件"),
      detail: probe.message,
      showClear: false,
    };
  }
  if (action === "resume") {
    return {
      kind: "info",
      title: t("可续传"),
      detail: probe.message,
      showClear: false,
    };
  }
  return {
    kind: "neutral",
    title: download ? t("可以开始下载") : t("可以开始上传"),
    detail: probe.message,
    showClear: false,
  };
}

function renderTransferStatePanel(view: TransferStateView) {
  transferStatePanel.classList.remove("hidden", "info", "warn", "error", "neutral");
  transferStatePanel.classList.add(view.kind);
  statePanelTitle.textContent = view.title;
  statePanelDetail.textContent = view.detail ?? "";
  statePanelDetail.classList.toggle("hidden", !view.detail);
  statePanelHint.textContent = view.hint ?? "";
  statePanelHint.classList.toggle("hidden", !view.hint);
  clearCheckpointBtn.classList.toggle("hidden", !view.showClear);
}

function hideTransferStatePanel() {
  transferStatePanel.classList.add("hidden");
  clearCheckpointBtn.classList.add("hidden");
}

function updateTransferStatePanel() {
  renderPendingTransfers();
  if (isTransferring) {
    hideTransferStatePanel();
    return;
  }

  const cp = getActiveCheckpoint();
  if (cp && cp.status !== "completed") {
    renderTransferStatePanel(analyzeCheckpointState(cp, isDownloadMode()));
    return;
  }

  if (lastProbe) {
    renderTransferStatePanel(analyzeProbeState(lastProbe, isDownloadMode()));
    return;
  }

  hideTransferStatePanel();
}

function setLastProbe(probe: RemoteUploadProbe | RemoteDownloadProbe | null) {
  lastProbe = probe;
  updateTransferStatePanel();
}

function clearLastProbe() {
  lastProbe = null;
  updateTransferStatePanel();
}

function showDownloadProbeResult(probe: RemoteDownloadProbe) {
  setLastProbe(probe);
}

function showProbeResult(probe: RemoteUploadProbe) {
  setLastProbe(probe);
}

function clearProbeResult() {
  clearLastProbe();
}

/** 用户正在编辑路径时静默丢弃预检结果，不触发窗口自适应 */
function invalidateProbeOnEdit() {
  if (!lastProbe) return;
  lastProbe = null;
  hideTransferStatePanel();
}

let confirmResolver: ((value: boolean) => void) | null = null;
let confirmClosing = false;

function openConfirmDialog() {
  confirmClosing = false;
  confirmOk.disabled = false;
  confirmCancel.disabled = false;
  confirmOverlay.classList.remove("hidden", "is-visible");
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      confirmOverlay.classList.add("is-visible");
      confirmOk.focus();
    });
  });
}

function showConfirm(
  title: string,
  body: string,
  labels: { ok?: string; cancel?: string } = {},
): Promise<boolean> {
  if (confirmResolver) {
    const stale = confirmResolver;
    confirmResolver = null;
    stale(false);
  }

  confirmTitle.textContent = title;
  confirmBody.textContent = body;
  // 按钮文案默认「确定 / 取消」；需要更明确的动作时（如「继续传输 / 稍后」）由调用方指定。
  confirmOk.textContent = labels.ok ?? t("确定");
  confirmCancel.textContent = labels.cancel ?? t("取消");
  openConfirmDialog();

  return new Promise((resolve) => {
    confirmResolver = resolve;
  });
}

function closeConfirm(result: boolean) {
  if (confirmClosing || confirmOverlay.classList.contains("hidden")) {
    return;
  }

  confirmClosing = true;
  confirmOk.disabled = true;
  confirmCancel.disabled = true;
  confirmOverlay.classList.remove("is-visible");

  window.setTimeout(() => {
    confirmOverlay.classList.add("hidden");
    confirmClosing = false;
    const resolve = confirmResolver;
    confirmResolver = null;
    resolve?.(result);
  }, CONFIRM_ANIM_MS);
}

let remoteBrowseCurrent: RemoteDirListing | null = null;
let remoteBrowseLoading = false;

function updateRemoteBrowseModeUi() {
  const download = isDownloadMode();
  remoteBrowseTitle.textContent = download ? t("浏览远端文件") : t("浏览远端目录");
  remoteBrowseSelectDirBtn.classList.toggle("hidden", download);
  remoteBrowseSelectDirBtn.textContent = t("选择此目录");
  remoteBrowseHint.textContent = download
    ? t("双击文件夹进入；单击文件选中")
    : t("双击文件夹进入；双击文件选中");
}

function openRemoteBrowseOverlay() {
  remoteBrowseOverlay.classList.remove("hidden", "is-visible");
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      remoteBrowseOverlay.classList.add("is-visible");
    });
  });
}

function closeRemoteBrowseOverlay() {
  remoteBrowseOverlay.classList.remove("is-visible");
  window.setTimeout(() => {
    remoteBrowseOverlay.classList.add("hidden");
    remoteBrowseList.replaceChildren();
    remoteBrowseStatus.textContent = "";
    remoteBrowseCurrent = null;
    hideRemoteBrowseNewDirForm();
  }, CONFIRM_ANIM_MS);
}

function showRemoteBrowseNewDirForm() {
  remoteBrowseNewDirRow.classList.remove("hidden");
  remoteBrowseNewDirName.value = "";
  remoteBrowseNewDirName.focus();
}

function hideRemoteBrowseNewDirForm() {
  remoteBrowseNewDirRow.classList.add("hidden");
  remoteBrowseNewDirName.value = "";
}

function setRemoteBrowseStatus(message: string, kind: "normal" | "error" = "normal") {
  remoteBrowseStatus.textContent = message;
  remoteBrowseStatus.classList.toggle("error", kind === "error");
}

function renderRemoteBrowseListing(listing: RemoteDirListing) {
  remoteBrowseCurrent = listing;
  remoteBrowsePath.value = listing.path;
  remoteBrowseUpBtn.disabled = !listing.parent;

  remoteBrowseList.replaceChildren();
  if (listing.entries.length === 0) {
    const empty = document.createElement("div");
    empty.className = "remote-browse-empty";
    empty.textContent = t("此目录为空");
    remoteBrowseList.appendChild(empty);
    return;
  }

  for (const entry of listing.entries) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = `remote-browse-item${entry.is_dir ? " is-dir" : " is-file"}`;
    row.dataset.path = entry.path;
    row.dataset.isDir = entry.is_dir ? "1" : "0";

    const icon = document.createElement("span");
    icon.className = "remote-browse-icon";
    // 图标是本地静态 SVG 常量，不含远端数据，可安全用 innerHTML。
    icon.innerHTML = entry.is_dir ? icons.folder : icons.file;

    const name = document.createElement("span");
    name.className = "remote-browse-name";
    name.textContent = entry.name;

    const meta = document.createElement("span");
    meta.className = "remote-browse-meta";
    meta.textContent = entry.is_dir ? t("目录") : formatBytes(entry.size);

    row.append(icon, name, meta);
    row.addEventListener("click", () => {
      remoteBrowseList
        .querySelectorAll(".remote-browse-item.selected")
        .forEach((el) => el.classList.remove("selected"));
      row.classList.add("selected");
      if (!entry.is_dir && isDownloadMode()) {
        applyRemoteBrowseSelection(entry.path, false);
      }
    });
    row.addEventListener("dblclick", (event) => {
      event.preventDefault();
      void handleRemoteBrowseEntryActivate(entry);
    });
    remoteBrowseList.appendChild(row);
  }
}

async function handleRemoteBrowseEntryActivate(entry: RemoteDirEntry) {
  if (entry.is_dir) {
    await loadRemoteBrowseDir(entry.path);
    return;
  }

  if (isDownloadMode()) {
    applyRemoteBrowseSelection(entry.path, false);
    return;
  }

  remotePathInput.value = entry.path;
  updateResolvedPathHint();
  clearProbeResult();
  scheduleBackgroundProbe();
  closeRemoteBrowseOverlay();
}

async function createRemoteBrowseDir() {
  const dirName = remoteBrowseNewDirName.value.trim();
  if (!dirName) {
    setRemoteBrowseStatus(t("请输入目录名称"), "error");
    return;
  }

  const parentPath = remoteBrowseCurrent?.path ?? remoteBrowsePath.value.trim();
  if (!parentPath) {
    setRemoteBrowseStatus(t("请先进入父目录"), "error");
    return;
  }

  remoteBrowseCreateDirBtn.disabled = true;
  setRemoteBrowseStatus(t("正在创建…"));

  try {
    await prepareConnection();
    const listing = await invoke<RemoteDirListing>("create_remote_dir", {
      request: {
        ...connectionPayload(),
        parent_path: parentPath,
        dir_name: dirName,
      },
    });
    hideRemoteBrowseNewDirForm();
    renderRemoteBrowseListing(listing);
    setRemoteBrowseStatus(t("已创建「{dirName}」", { dirName }));
  } catch (error) {
    setRemoteBrowseStatus(describeError(error), "error");
  } finally {
    remoteBrowseCreateDirBtn.disabled = false;
  }
}

async function loadRemoteBrowseDir(path?: string) {
  if (remoteBrowseLoading) return;
  remoteBrowseLoading = true;
  browseRemoteBtn.disabled = true;
  remoteBrowseGoBtn.disabled = true;
  remoteBrowseUpBtn.disabled = true;
  setRemoteBrowseStatus(t("正在加载…"));

  try {
    await prepareConnection();
    const listing = await invoke<RemoteDirListing>("list_remote_dir", {
      request: {
        ...connectionPayload(),
        path: path?.trim() || remoteBrowsePath.value.trim() || null,
      },
    });
    renderRemoteBrowseListing(listing);
    setRemoteBrowseStatus(t("{length} 项", { length: listing.entries.length }));
  } catch (error) {
    setRemoteBrowseStatus(describeError(error), "error");
  } finally {
    remoteBrowseLoading = false;
    browseRemoteBtn.disabled = isTransferring;
    remoteBrowseGoBtn.disabled = false;
    if (remoteBrowseCurrent?.parent) {
      remoteBrowseUpBtn.disabled = false;
    }
  }
}

function applyRemoteBrowseSelection(path: string, isDir: boolean) {
  if (isDownloadMode()) {
    if (isDir) return;
    remotePathInput.value = path;
  } else {
    remotePathInput.value = path.endsWith("/") ? path : `${path}/`;
  }
  updateResolvedPathHint();
  clearProbeResult();
  scheduleBackgroundProbe();
  closeRemoteBrowseOverlay();
}

async function openRemoteBrowseDialog() {
  if (isTransferring) return;

  try {
    await ensureCredentialsLoaded();
    validateConnectionBeforeTransfer();
  } catch (error) {
    setStatus(describeError(error), "error");
    return;
  }

  updateRemoteBrowseModeUi();
  hideRemoteBrowseNewDirForm();
  openRemoteBrowseOverlay();
  const initialPath = remotePathInput.value.trim() || undefined;
  remoteBrowsePath.value = initialPath ?? "";
  await loadRemoteBrowseDir(initialPath);
}

function uploadPayload(forceOverwrite: boolean) {
  return {
    ...connectionPayload(),
    local_path: localPathInput.value.trim(),
    remote_path: remotePathInput.value.trim(),
    strict_chunk_verify: isReliableMode(),
    force_overwrite: forceOverwrite,
  };
}

async function probeRemoteDownload(): Promise<RemoteDownloadProbe> {
  validateConnectionBeforeTransfer();
  const localPath = localPathInput.value.trim();
  const remotePath = remotePathInput.value.trim();
  if (!localPath) throw new Error(t("请填写本地保存路径"));
  if (!remotePath) throw new Error(t("请填写远端文件路径"));

  return invoke<RemoteDownloadProbe>("probe_remote_download", {
    request: {
      ...connectionPayload(),
      local_path: localPath,
      remote_path: remotePath,
    },
  });
}

async function confirmDownloadAction(probe: RemoteDownloadProbe): Promise<boolean> {
  showDownloadProbeResult(probe);

  if (probe.action === "verify_retry" || probe.action === "resume" || probe.action === "new") {
    return true;
  }

  if (forceOverwriteInput.checked) {
    return showConfirm(
      t("强制覆盖下载"),
      t("将删除本地已有文件并从头重下，是否继续？"),
    );
  }

  if (probe.action === "full_redownload") {
    return showConfirm(
      t("需要从头重下"),
      t("{message}\n\n将删除本地已有文件并重新下载，是否继续？", { message: probe.message }),
    );
  }

  if (probe.action === "overwrite") {
    return showConfirm(
      t("覆盖本地文件"),
      t("{message}\n\n本地已有同名文件，继续将覆盖重下，是否继续？", { message: probe.message }),
    );
  }

  return true;
}

function downloadPayload(forceOverwrite: boolean) {
  return {
    ...connectionPayload(),
    local_path: localPathInput.value.trim(),
    remote_path: remotePathInput.value.trim(),
    strict_chunk_verify: isReliableMode(),
    force_overwrite: forceOverwrite,
  };
}

async function invokeStartDownload(forceOverwrite: boolean) {
  try {
    await invoke("start_download", {
      request: downloadPayload(forceOverwrite),
    });
  } catch (error) {
    const message = describeError(error);
    if (!isTransferring && matchesBackend(message, BACKEND_TEXT.downloadRunning)) {
      await invoke("reset_download_state");
      await invoke("start_download", {
        request: downloadPayload(forceOverwrite),
      });
      return;
    }
    throw error;
  }
}

async function startDownloadFlow() {
  await prepareConnection();
  const probe = await probeRemoteDownload();
  const confirmed = await confirmDownloadAction(probe);
  if (!confirmed) return;

  const forceOverwrite =
    probe.action === "full_redownload" || forceOverwriteInput.checked;
  runRemotePath = probe.resolved_remote_path;
  runPostCommandTemplate = postCommandInput.value.trim();

  setTransferring(true);
  retryCount = 0;
  resetSpeedTracker();
  isConnecting = true;
  setProgressState("uploading");
  applyActiveTransferBadge("download");
  showProgressSpeed(t("正在建立 SSH 连接…"));

  await invokeStartDownload(forceOverwrite);
}

async function probeRemoteUpload(): Promise<RemoteUploadProbe> {
  validateConnectionBeforeTransfer();
  const localPath = localPathInput.value.trim();
  const remotePath = remotePathInput.value.trim();
  if (!localPath) throw new Error(t("请选择本地文件"));
  if (!remotePath) throw new Error(t("请填写远端路径"));

  return invoke<RemoteUploadProbe>("probe_remote_upload", {
    request: {
      ...connectionPayload(),
      local_path: localPath,
      remote_path: remotePath,
    },
  });
}

async function confirmUploadAction(probe: RemoteUploadProbe): Promise<boolean> {
  showProbeResult(probe);

  if (probe.action === "verify_retry" || probe.action === "resume" || probe.action === "new") {
    return true;
  }

  if (forceOverwriteInput.checked) {
    return showConfirm(
      t("强制覆盖上传"),
      t("将删除远端已有文件并从头重传，是否继续？"),
    );
  }

  if (probe.action === "full_reupload") {
    return showConfirm(
      t("需要从头重传"),
      t("{message}\n\n将删除远端已有文件并重新上传，是否继续？", { message: probe.message }),
    );
  }

  if (probe.action === "overwrite") {
    return showConfirm(
      t("覆盖远端文件"),
      t("{message}\n\n远端已有同名文件，继续将覆盖重传，是否继续？", { message: probe.message }),
    );
  }

  return true;
}

async function invokeStartUpload(forceOverwrite: boolean) {
  try {
    await invoke("start_upload", {
      request: uploadPayload(forceOverwrite),
    });
  } catch (error) {
    const message = describeError(error);
    if (!isTransferring && matchesBackend(message, BACKEND_TEXT.uploadRunning)) {
      await invoke("reset_upload_state");
      await invoke("start_upload", {
        request: uploadPayload(forceOverwrite),
      });
      return;
    }
    throw error;
  }
}

function isQueueSelection(): boolean {
  return !isDownloadMode() && selectedUploads.length > 1;
}

function setSelectedUploads(paths: string[]) {
  selectedUploads = paths.filter(Boolean);
  localPathInput.value = selectedUploads[0] ?? "";
  updateDropZoneState();
  updateResolvedPathHint();
  clearProbeResult();
  // 多文件时只预检第一个会产生误导，等开始时逐个预检。
  if (selectedUploads.length === 1) scheduleBackgroundProbe();
}

async function startUploadFlow() {
  await prepareConnection();
  const outcome = await startOneUpload();
  if (outcome === "identical") await finishIdenticalUpload();
}

/**
 * 远端已有完全相同的文件：不传输，直接按「已送达」收尾。
 * 传完后命令照常执行——文件确实已在服务器上且校验一致，部署流程不应因为「没变化」而中断。
 */
async function finishIdenticalUpload() {
  runDirection = "upload";
  beginRunVisuals();
  showTransferView("run");
  const size = lastProbe?.local_size ?? 0;
  blockMap.update(size, size);
  blockMap.setPhase("completed");
  showVerifySummary(t("远端 SHA-256 与本地一致"));
  await finishSuccessfulRun(
    t("远端已是最新"),
    t("远端已有完全相同的文件，无需上传"),
    { elapsedMs: 0, movedBytes: 0, retries: 0 },
    [runRemotePath],
  );
}

/**
 * 预检、确认、启动一个上传。返回 false 表示用户在确认框里放弃了这个文件。
 * 单文件与队列共用：队列只是对每个文件依次调用它，续传、校验等行为完全一致。
 */
type UploadStart = "started" | "declined" | "identical";

async function startOneUpload(): Promise<UploadStart> {
  const probe = await probeRemoteUpload();
  // 空间不足时直接拦下：传到 90% 才因磁盘满失败，比现在就报错糟糕得多。
  if (probe.action === "insufficient_space") {
    showProbeResult(probe);
    throw new Error(probe.message);
  }
  // 勾了「强制从头覆盖」说明用户明确要重传，此时不走跳过逻辑。
  if (probe.action === "identical" && !forceOverwriteInput.checked) {
    showProbeResult(probe);
    runRemotePath = probe.resolved_remote_path;
    runPostCommandTemplate = postCommandInput.value.trim();
    return "identical";
  }
  const confirmed = await confirmUploadAction(probe);
  if (!confirmed) return "declined";

  const forceOverwrite =
    probe.action === "full_reupload" || forceOverwriteInput.checked;
  runRemotePath = probe.resolved_remote_path;
  runPostCommandTemplate = postCommandInput.value.trim();

  setTransferring(true);
  retryCount = 0;
  resetSpeedTracker();
  isConnecting = true;
  setProgressState("uploading");
  applyActiveTransferBadge("upload");
  showProgressSpeed(t("正在建立 SSH 连接…"));

  await invokeStartUpload(forceOverwrite);
  return "started";
}

/**
 * 多文件上传：逐个走单文件流程。某个文件失败不影响后面的文件；用户取消则整个队列停止。
 * 传完后命令只在队列结束后执行一次——部署场景下应当全部传完再重启服务。
 */
async function startQueueFlow() {
  await prepareConnection();
  // 多个文件必须传到目录里，否则会互相覆盖成同一个远端文件。
  const remoteDir = remotePathInput.value.trim();
  if (!remoteDir) throw new Error(t("请填写远端目录"));
  if (!remoteDir.endsWith("/")) remotePathInput.value = `${remoteDir}/`;

  const queueStartedAt = Date.now();
  const template = postCommandInput.value.trim();
  const queue = {
    items: selectedUploads.map<QueueItem>((local) => ({ local, remote: "", status: "pending" })),
    index: 0,
  };
  activeQueue = queue;

  try {
    for (let i = 0; i < queue.items.length; i++) {
      const item = queue.items[i];
      queue.index = i;
      localPathInput.value = item.local;
      item.status = "running";
      const step = new Promise<RunStepResult>((resolve) => {
        resolveQueueStep = resolve;
      });
      let started: UploadStart;
      try {
        started = await startOneUpload();
      } catch (error) {
        resolveQueueStep = null;
        setTransferring(false);
        item.status = "failed";
        item.message = clarifyTransferError(describeError(error));
        continue;
      }
      if (started === "declined") {
        resolveQueueStep = null;
        item.status = "skipped";
        item.message = t("已跳过");
        continue;
      }
      if (started === "identical") {
        // 与远端完全一致视为已送达：计入成功，队列结束后的命令照常执行。
        resolveQueueStep = null;
        item.status = "done";
        item.remote = runRemotePath;
        item.message = t("已是最新，跳过");
        continue;
      }
      item.remote = runRemotePath;
      const result = await step;
      item.stats = result.stats;
      if (result.status === "completed") {
        item.status = "done";
      } else if (result.status === "failed") {
        item.status = "failed";
        item.message = clarifyTransferError(result.message);
      } else {
        item.status = "cancelled";
        item.message = t("已取消，断点已保留");
        for (const rest of queue.items.slice(i + 1)) {
          rest.status = "cancelled";
          rest.message = t("未开始");
        }
        break;
      }
    }
  } finally {
    resolveQueueStep = null;
    activeQueue = null;
    queueChip.classList.add("hidden");
  }
  await finishQueue(queue.items, template, queueStartedAt);
}

/** 汇总队列结果：全部成功才算「完成」，部分失败为「警告」，用户取消为「已暂停」。 */
async function finishQueue(items: QueueItem[], template: string, startedAt: number) {
  const done = items.filter((i) => i.status === "done");
  const stats: RunStats = {
    elapsedMs: Date.now() - startedAt,
    movedBytes: items.reduce((sum, i) => sum + (i.stats?.movedBytes ?? 0), 0),
    retries: items.reduce((sum, i) => sum + (i.stats?.retries ?? 0), 0),
  };
  const cancelled = items.some((i) => i.status === "cancelled");
  const total = items.length;
  renderQueueResultFiles(items);
  // 选择只保留没成功的文件：之后点「重试 / 继续传输」不会把已完成的文件再传一遍。
  const remaining = items.filter((i) => i.status !== "done").map((i) => i.local);
  selectedUploads = remaining.length > 0 ? remaining : items.map((i) => i.local);
  localPathInput.value = selectedUploads[0];
  updateDropZoneState();
  updateResolvedPathHint();

  if (done.length === total) {
    runPostCommandTemplate = template;
    await finishSuccessfulRun(
      t("{total} 个文件全部上传完成", { total }),
      t("已传到 {p1}:{p2}", { p1: hostInput.value.trim(), p2: remoteDirOf(done[0].remote) }),
      stats,
      done.map((i) => i.remote),
    );
    return;
  }
  // 部分失败时不执行命令：拿着不完整的文件去重启服务往往比不重启更糟。
  showCommandResult(null);
  // 校验徽章属于最后一个成功的文件，放在整体未成功的结果里会误导。
  hideVerifySummary();
  const failedCount = items.filter((i) => i.status === "failed").length;
  const otherCount = total - done.length - failedCount;
  const summary = [
    t("成功 {length} 个", { length: done.length }),
    failedCount > 0 ? t("失败 {failedCount} 个", { failedCount }) : "",
    otherCount > 0 ? t("未完成 {otherCount} 个", { otherCount }) : "",
  ]
    .filter(Boolean)
    .join("，");
  const commandNote = template ? t("\n因有文件未成功，传完后命令没有执行") : "";
  if (cancelled) {
    showResult("paused", t("批量上传已取消"), `${summary}${commandNote}`, stats);
  } else if (done.length === 0) {
    showResult("error", t("批量上传失败"), `${summary}${commandNote}`, stats);
  } else {
    showResult("warn", t("部分文件上传失败"), `${summary}${commandNote}`, stats, false);
  }
}

const QUEUE_STATUS_LABEL: Record<QueueItemStatus, string> = {
  pending: t("未开始"),
  running: t("传输中"),
  done: t("完成"),
  failed: t("失败"),
  skipped: t("已跳过"),
  cancelled: t("已取消"),
};

function renderQueueResultFiles(items: QueueItem[] | null) {
  if (!items) {
    resultFiles.classList.add("hidden");
    resultFiles.replaceChildren();
    return;
  }
  resultFiles.classList.remove("hidden");
  resultFiles.replaceChildren(
    ...items.map((item) => {
      const li = document.createElement("li");
      li.className = item.status;
      const mark = document.createElement("span");
      mark.className = "file-mark";
      const name = document.createElement("span");
      name.className = "file-name";
      name.textContent = fileNameOf(item.local);
      name.title = item.local;
      const status = document.createElement("span");
      status.className = "file-status";
      status.textContent = item.message ?? QUEUE_STATUS_LABEL[item.status];
      status.title = status.textContent;
      li.append(mark, name, status);
      return li;
    }),
  );
}

function updateAuthFields() {
  const isKey = authTypeSelect.value === "key";
  keyFields.classList.toggle("hidden", !isKey);
  passwordFields.classList.toggle("hidden", isKey);
  updateConnectionSummary();
  updateTransferStatePanel();
  updateAuthHintVisibility();
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function formatTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString(locale(), {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** 最近使用的目录最多列几个：太多会把收藏挤出视野。 */
const RECENT_DIR_LIMIT = 6;

function currentProfile(): ConnectionProfile | undefined {
  return savedProfiles.find((p) => p.id === profileSelect.value);
}

/** 收藏的是目录：填的是文件路径时取其所在目录，统一以 / 结尾，便于直接作为上传目标。 */
function remoteDirOf(path: string): string {
  const trimmed = path.trim();
  if (!trimmed) return "";
  if (trimmed.endsWith("/")) return trimmed;
  const slash = trimmed.lastIndexOf("/");
  return slash >= 0 ? trimmed.slice(0, slash + 1) : "";
}

function updateBookmarkButton() {
  const profile = currentProfile();
  const dir = remoteDirOf(remotePathInput.value);
  const bookmarked = Boolean(profile && dir && profile.bookmarks.includes(dir));
  bookmarkBtn.disabled = !profile || !dir || isTransferring;
  bookmarkBtn.classList.toggle("is-active", bookmarked);
  bookmarkBtn.innerHTML = bookmarked ? icons.starFilled : icons.star;
  bookmarkBtn.title = !profile
    ? t("保存服务器后才能收藏目录")
    : !dir
      ? t("填写远端目录后可收藏")
      : bookmarked
        ? t("取消收藏 {dir}", { dir })
        : t("收藏 {dir}", { dir });
}

async function saveProfileExtras(
  profile: ConnectionProfile,
  changes: { bookmarks?: string[]; post_commands?: string[] },
) {
  const updated = await invoke<ConnectionProfile>("update_connection_profile_extras", {
    request: { id: profile.id, ...changes },
  });
  const index = savedProfiles.findIndex((p) => p.id === updated.id);
  if (index >= 0) savedProfiles[index] = updated;
  return updated;
}

async function toggleBookmark() {
  const profile = currentProfile();
  const dir = remoteDirOf(remotePathInput.value);
  if (!profile || !dir) return;
  const bookmarks = profile.bookmarks.includes(dir)
    ? profile.bookmarks.filter((b) => b !== dir)
    : [dir, ...profile.bookmarks];
  try {
    await saveProfileExtras(profile, { bookmarks });
    updateBookmarkButton();
  } catch (error) {
    setStatus(describeError(error), "error");
  }
}

/** 最近目录从传输历史推导，不额外存储：历史本来就记录了每次传到哪里。 */
function recentDirsForCurrentServer(exclude: string[]): string[] {
  const host = hostInput.value.trim();
  const port = Number(portInput.value) || 22;
  const user = usernameInput.value.trim();
  const dirs: string[] = [];
  for (const entry of historyEntries) {
    if (entry.host !== host || entry.port !== port || entry.username !== user) continue;
    const dir = remoteDirOf(entry.remote_path);
    if (dir && !dirs.includes(dir) && !exclude.includes(dir)) dirs.push(dir);
    if (dirs.length >= RECENT_DIR_LIMIT) break;
  }
  return dirs;
}

function useRemotePath(path: string) {
  remotePathInput.value = path;
  updateResolvedPathHint();
  clearProbeResult();
  scheduleBackgroundProbe();
  updateBookmarkButton();
  remotePathInput.focus();
}

function openPathMenu() {
  const profile = currentProfile();
  const bookmarks = profile?.bookmarks ?? [];
  const bookmarkItems: PopoverItem[] = bookmarks.map((dir) => ({
    label: dir,
    onSelect: () => useRemotePath(dir),
    onRemove: () => {
      if (!profile) return;
      void saveProfileExtras(profile, { bookmarks: bookmarks.filter((b) => b !== dir) }).then(() => {
        updateBookmarkButton();
        openPathMenu();
      });
    },
  }));
  const recentItems: PopoverItem[] = recentDirsForCurrentServer(bookmarks).map((dir) => ({
    label: dir,
    onSelect: () => useRemotePath(dir),
  }));
  pathMenu.show(
    [
      { heading: t("收藏"), items: bookmarkItems },
      { heading: t("最近使用"), items: recentItems },
    ],
    profile
      ? t("还没有常用目录\n填好远端目录后点输入框里的 ☆ 收藏")
      : t("这台服务器还没保存\n到「服务器」页保存后即可收藏目录"),
  );
}

/** 本次运行中已确认可信的 host:port，避免每次浏览目录/预检都多一次握手（弱网下很贵）。 */
const trustedHostsThisSession = new Set<string>();

/**
 * 任何会发送凭据的连接前调用：未知主机先展示指纹让用户确认，再写入应用 known_hosts。
 * 后端 connect_session 仍会强制校验，这里只是把「拒绝连接」变成可操作的确认流程。
 */
async function ensureHostTrusted(): Promise<void> {
  const { host, port } = connectionPayload();
  const hostId = `${host}:${port}`;
  if (trustedHostsThisSession.has(hostId)) return;

  const info = await invoke<HostKeyInfo>("check_host_key", { request: { host, port } });
  if (info.status === "changed") {
    throw new Error(
      t("服务器 {hostId} 的指纹与已记录的不一致（当前 {fingerprint}），可能遭遇中间人攻击，已拒绝连接。\n", { hostId, fingerprint: info.fingerprint }) +
        t("如确认服务器重装过系统，请从 ~/.ssh/known_hosts 或 ~/Library/Application Support/sshutil/known_hosts 中删除旧记录后重试。"),
    );
  }
  if (info.status === "unknown") {
    const trusted = await showConfirm(
      t("首次连接该服务器"),
      `${hostId}\n${info.key_type}\n${info.fingerprint}\n\n` +
        t("请与服务器管理员核对上面的指纹（服务器上可运行 ssh-keygen -lf /etc/ssh/ssh_host_*_key.pub）。确认信任后将记住该服务器。"),
    );
    if (!trusted) throw new Error(t("未信任该服务器，已取消连接"));
    await invoke("trust_host_key", {
      request: { host, port, fingerprint: info.fingerprint },
    });
  }
  trustedHostsThisSession.add(hostId);
}

function connectionPayload() {
  return {
    host: hostInput.value.trim(),
    port: Number(portInput.value) || 22,
    username: usernameInput.value.trim(),
    auth_type: authTypeSelect.value,
    password: passwordInput.value,
    key_path: keyPathInput.value.trim(),
    key_passphrase: keyPassphraseInput.value,
  };
}

function setBadge(
  text: string,
  kind: "idle" | "uploading" | "retrying" | "verifying" | "error" | "stalled" = "idle",
) {
  progressBadge.textContent = text;
  progressBadge.className = "badge";
  if (kind !== "idle") progressBadge.classList.add(kind);
}

/**
 * 后端错误文本中的固定片段（中英文各一份）。后端按界面语言出文案，但切换语言前产生的消息
 * 仍是旧语言，所以两种都要认。与 src-tauri 中对应文案保持一致，修改时两边同步。
 */
const BACKEND_TEXT = {
  uploadRunning: ["已有上传任务在进行中", "An upload is already in progress"],
  downloadRunning: ["已有下载任务在进行中", "A download is already in progress"],
  remotePath: ["远端路径错误", "远端路径「", "Remote path error", 'Remote path "'],
  passwordMissing: ["请填写密码", "Please enter a password"],
  authFailed: ["认证失败", "服务器拒绝了提供的凭据", "Authentication failed", "The server rejected the credentials"],
  sftpError: ["SFTP 错误", "SFTP error"],
} as const;

function matchesBackend(message: string, fragments: readonly string[]): boolean {
  return fragments.some((fragment) => message.includes(fragment));
}

/** 只取错误信息本身：直接 String(new Error(x)) 会在界面上带出英文 "Error: " 前缀。 */
function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function clarifyTransferError(message: string): string {
  if (matchesBackend(message, BACKEND_TEXT.remotePath)) {
    return message;
  }
  if (matchesBackend(message, BACKEND_TEXT.passwordMissing)) {
    return t("未填写密码。请到「服务器」页填写密码");
  }
  if (
    matchesBackend(message, BACKEND_TEXT.authFailed) &&
    authTypeSelect.value === "password" &&
    !passwordInput.value
  ) {
    return t("未填写密码。请到「服务器」页填写密码");
  }
  if (matchesBackend(message, BACKEND_TEXT.authFailed)) {
    return t("SSH 认证失败，密码或私钥可能不正确。\n{message}", { message });
  }
  const remote = remotePathInput.value.trim();
  if (/no such file|sftp\(2\)/i.test(message) && remote) {
    return t("远端路径错误：{remote}\n该路径或上级目录在服务器上不存在（本地文件不受影响）", { remote });
  }
  if (matchesBackend(message, BACKEND_TEXT.sftpError) && remote && !message.includes(remote)) {
    return t("远端路径错误：{remote}\n{message}", { remote, message });
  }
  return message;
}

function setStatus(message: string, kind: "normal" | "error" | "retry" = "normal") {
  const text = kind === "error" ? clarifyTransferError(message) : message;
  statusText.textContent = text;
  statusText.classList.remove("error", "retry");
  if (kind === "error") statusText.classList.add("error");
  if (kind === "retry") statusText.classList.add("retry");
}

function setProgressState(state: "idle" | "uploading" | "retrying" | "verifying") {
  progressWrap.classList.remove("uploading", "retrying", "verifying");
  if (state === "uploading") progressWrap.classList.add("uploading");
  if (state === "retrying") progressWrap.classList.add("retrying");
  if (state === "verifying") progressWrap.classList.add("verifying");
}

/** 传输进行中徽章：连接中 / 重连无响应 / 正常传输 */
function applyActiveTransferBadge(direction: TransferDirection) {
  const badgeActive = direction === "upload" ? t("上传中") : t("下载中");
  if (isConnecting) {
    setProgressState("uploading");
    setBadge(t("连接中"), "retrying");
    return;
  }
  if (progressWrap.classList.contains("stalled")) {
    setProgressState("retrying");
    setBadge(t("等待响应"), "stalled");
    return;
  }
  if (isReconnecting) {
    setProgressState("retrying");
    setBadge(t("重试中"), "retrying");
    return;
  }
  setProgressState("uploading");
  setBadge(badgeActive, "uploading");
}

/** 当前传输所处阶段，驱动分块地图的动效与侧边栏迷你进度的配色。 */
function resolveMapPhase(): BlockMapPhase {
  if (isConnecting) return "connecting";
  if (isFullVerifying) return "verifying";
  if (
    isReconnecting ||
    progressWrap.classList.contains("stalled") ||
    (lastProgressEventAt > 0 && Date.now() - lastProgressEventAt >= SPEED_STALE_MS)
  ) {
    return "waiting";
  }
  return "transferring";
}

/**
 * 同步所有「活的」指示：分块地图阶段、侧边栏迷你进度、导航上的进行中圆点。
 * 侧边栏迷你进度让用户切到其他页面时仍能看到传输状态，不必守在传输页。
 */
function refreshLiveVisuals() {
  const phase = isTransferring ? resolveMapPhase() : null;
  if (phase) blockMap.setPhase(phase);

  const waiting = phase === "waiting" || phase === "connecting";
  navTransferLive.classList.toggle("hidden", !isTransferring);
  navTransferLive.classList.toggle("is-waiting", waiting);
  miniTransfer.classList.toggle("hidden", !isTransferring || currentPage === "transfer");
  miniTransfer.classList.toggle("is-waiting", waiting);
  if (!isTransferring) return;

  const verb = runDirection === "download" ? t("下载") : t("上传");
  const queuePart = activeQueue ? ` ${activeQueue.index + 1}/${activeQueue.items.length}` : "";
  miniTransferLabel.textContent =
    (phase === "connecting" ? t("连接中") : phase === "verifying" ? t("校验中") : waiting ? t("重连中") : t("{verb}中", { verb })) +
    queuePart;
  const pct = progressTotalBytes > 0 ? (progressTransferredBytes / progressTotalBytes) * 100 : 0;
  miniTransferPct.textContent = `${Math.floor(pct)}%`;
  miniTransferFill.style.width = `${pct}%`;
  miniTransferSpeed.textContent = progressSpeed.textContent ?? "";
}

function estimateTransferSpeedBps(): number | null {
  if (speedSamples.length < 2) return null;
  const first = speedSamples[0];
  const last = speedSamples[speedSamples.length - 1];
  const deltaT = (last.t - first.t) / 1000;
  if (deltaT < 0.3) return null;
  const deltaBytes = last.bytes - first.bytes;
  if (deltaBytes <= 0) return 0;
  return deltaBytes / deltaT;
}

function hideVerifySummary() {
  verifySummary.classList.add("hidden");
  verifySummary.textContent = "";
}

function showVerifySummary(text: string) {
  verifySummary.textContent = text;
  verifySummary.classList.remove("hidden");
}

function applyThemePreference(mode: ThemePreference) {
  document.documentElement.dataset.theme = resolveTheme(mode);
  localStorage.setItem(THEME_STORAGE_KEY, mode);
}

function initThemePreference() {
  const saved = readThemePreference();
  const radio = document.querySelector<HTMLInputElement>(
    `input[name="themeMode"][value="${saved}"]`,
  );
  if (radio) radio.checked = true;
  applyThemePreference(saved);
  // 跟随系统时，系统切换明暗需要实时生效。
  systemDarkQuery.addEventListener("change", () => {
    if (readThemePreference() === "system") applyThemePreference("system");
  });
}

function formatProgressDetail(transferred: number, total: number, retries: number): string {
  // 百分比已经用大号数字单独展示，这里只给字节数与重连次数。
  const size = `${formatBytes(transferred)} / ${formatBytes(total)}`;
  return retries > 0 ? t("{size} · 已自动重连 {retries} 次", { size, retries }) : size;
}

function syncProgressStatsRow() {
  // 进行中视图的统计行固定显示（避免布局跳动），这里只同步侧边栏迷你进度的速度文字。
  miniTransferSpeed.textContent = progressSpeed.textContent ?? "";
}

function showProgressSpeed(text: string, stale = false) {
  progressSpeed.textContent = text;
  progressSpeed.classList.remove("hidden");
  progressSpeed.classList.toggle("stale", stale);
  syncProgressStatsRow();
}

/** 重试退避剩余秒数，供副文本展示 */
function formatRetryDelayHint(): string {
  if (retryScheduledAt <= 0 || retryDelayMs <= 0) return "";
  const remaining = Math.max(0, Math.ceil((retryScheduledAt + retryDelayMs - Date.now()) / 1000));
  return remaining > 0 ? t("约 {remaining}s 后重试", { remaining }) : "";
}

/** Stall/retry 副文本：动态计时，避免用户误以为界面卡死 */
function showWaitingSpeedText(elapsedMs: number) {
  const secs = Math.floor(elapsedMs / 1000);
  const delayHint = formatRetryDelayHint();
  const details: string[] = [];
  if (isReconnecting && retryCount > 0) details.push(t("第 {retryCount} 次重试", { retryCount }));
  if (delayHint) details.push(delayHint);
  details.push(t("已等待 {secs}s", { secs }));
  const prefix = isReconnecting ? t("重连中...") : t("等待响应...");
  showProgressSpeed(`${prefix} (${details.join(" · ")})`, true);
}

/** 刚进入重连时的副文本 */
function showReconnectingSpeedText() {
  const delayHint = formatRetryDelayHint();
  const details: string[] = [];
  if (retryCount > 0) details.push(t("第 {retryCount} 次重试", { retryCount }));
  if (delayHint) details.push(delayHint);
  const prefix = t("重连中...");
  showProgressSpeed(
    details.length > 0 ? `${prefix} (${details.join(" · ")})` : prefix,
    true,
  );
}

function hideProgressSpeed() {
  progressSpeed.classList.add("hidden");
  progressSpeed.classList.remove("stale");
  progressSpeed.textContent = "";
  syncProgressStatsRow();
}

function updateProgressDetail(transferred: number, total: number, retries = retryCount) {
  if (total > 0 && isTransferring) {
    progressDetail.textContent = formatProgressDetail(transferred, total, retries);
    progressDetail.classList.remove("hidden");
  } else if (!isTransferring) {
    progressDetail.classList.add("hidden");
  }
  syncProgressStatsRow();
}

function showTransferStatus(message: string, kind: "normal" | "error" | "retry" = "normal") {
  statusText.classList.remove("hidden");
  setStatus(message, kind);
}

function hideTransferStatus() {
  if (isTransferring) {
    statusText.classList.add("hidden");
    statusText.textContent = "";
  }
}

function clearProbeDebounce() {
  if (probeDebounceTimer !== null) {
    window.clearTimeout(probeDebounceTimer);
    probeDebounceTimer = null;
  }
}

function scheduleBackgroundProbe() {
  if (isTransferring) return;
  clearProbeDebounce();
  probeDebounceTimer = window.setTimeout(() => {
    probeDebounceTimer = null;
    void runBackgroundProbe();
  }, PROBE_DEBOUNCE_MS);
}

async function runBackgroundProbe() {
  if (isTransferring || isQueueSelection()) return;
  const localPath = localPathInput.value.trim();
  const remotePath = remotePathInput.value.trim();
  if (!localPath || !remotePath) return;

  const gen = ++backgroundProbeGeneration;

  try {
    validateConnectionBeforeTransfer();
    const probe = isDownloadMode()
      ? await probeRemoteDownload()
      : await probeRemoteUpload();
    if (gen !== backgroundProbeGeneration || isTransferring) return;
    setLastProbe(probe);
  } catch {
    // 预检失败时不打扰用户，点开始时会再报
  }
}

function restoreCredentialsFromProfile(host: string, port: number, username: string) {
  const profile = savedProfiles.find(
    (p) => p.host === host && p.port === port && p.username === username,
  );
  if (!profile) return;

  profileSelect.value = profile.id;
  profileNameInput.value = profile.name;

  // 这里由启动时自动恢复断点触发，不主动读钥匙串（会弹授权框）；
  // 真正连接前由 ensureCredentialsLoaded 按需补齐密码。

  restorePostCommand();
  setConnectionStatus(t("已匹配服务器「{name}」", { name: profile.name }), "ok");
  updateConnectionSummary();
}

function formatEtaSeconds(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "";
  if (seconds < 60) return t("{p1} 秒", { p1: Math.ceil(seconds) });
  if (seconds < 3600) return t("{p1} 分钟", { p1: Math.ceil(seconds / 60) });
  return t("{p1} 小时", { p1: (seconds / 3600).toFixed(1) });
}

function formatProgressPercent(transferred: number, total: number): string {
  if (total <= 0) return "";
  const pct = Math.min(100, Math.round((transferred / total) * 100));
  return `${pct}%`;
}

/**
 * 分块地图需要块内的细粒度进度（部分填充 + 当前块呼吸），所以每个进度事件都刷新；
 * 实际绘制由 BlockMap 按帧合并，事件再频繁也不会过度重绘。
 */
function updateProgress(transferred: number, total: number) {
  // 只在传输进行中驱动地图：空闲时恢复断点也会调用这里，不能把断点进度当成本次起点。
  if (!isTransferring) return;
  progressTotalBytes = total;
  progressTransferredBytes = transferred;
  if (runStartBytes === null) runStartBytes = transferred;
  progressPercent.textContent = formatProgressPercent(transferred, total) || "0%";
  blockMap.update(transferred, total);
  const perCell = blockMap.blocksPerCell;
  mapCaption.textContent =
    perCell > 1
      ? t("{p1} 块 · 每格 {perCell} MB", { p1: blockMap.blockCount.toLocaleString(), perCell })
      : t("{p1} 块 · 每块 1 MB", { p1: blockMap.blockCount.toLocaleString() });
  syncMapLegend();
  updateProgressDetail(transferred, total);
  refreshLiveVisuals();
}

/** 图例只列出地图上真实出现的状态：全新上传没有「之前已续传」，没断过线就不提「重连处」。 */
function syncMapLegend() {
  const perBlock = blockMap.verifiesPerBlock;
  mapLegend.classList.toggle("per-block", perBlock);
  legendDoneLabel.textContent = perBlock ? t("本次已传并校验") : t("本次已传（待最终校验）");
  legendResumed.classList.toggle("hidden", !blockMap.hasResumedPart);
  legendRetry.classList.toggle("hidden", blockMap.retryCount === 0);
}


function resetSpeedTracker() {
  lastProgressEventAt = 0;
  speedSamples = [];
  isReconnecting = false;
  isConnecting = false;
  isChunkVerifying = false;
  isFullVerifying = false;
  retryDelayMs = 0;
  retryScheduledAt = 0;
  hideProgressSpeed();
  progressWrap.classList.remove("stalled");
  progressDetail.classList.add("hidden");
  syncProgressStatsRow();
}

function markProgressActivity() {
  lastProgressEventAt = Date.now();
  isConnecting = false;
  isChunkVerifying = false;
  retryDelayMs = 0;
  retryScheduledAt = 0;
  const wasStalled = progressWrap.classList.contains("stalled");
  const wasReconnecting = isReconnecting;
  progressWrap.classList.remove("stalled");
  progressSpeed.classList.remove("stale");
  isReconnecting = false;
  if (isTransferring && (wasStalled || wasReconnecting)) {
    applyActiveTransferBadge(isDownloadMode() ? "download" : "upload");
    refreshLiveVisuals();
  }
}

function recordSpeedSample(bytes: number) {
  markProgressActivity();
  const now = Date.now();
  speedSamples.push({ t: now, bytes });
  while (speedSamples.length > 0 && now - speedSamples[0].t > SPEED_SAMPLE_WINDOW_MS) {
    speedSamples.shift();
  }
}

function refreshSpeedDisplay() {
  if (!isTransferring) return;

  const elapsed = Date.now() - lastProgressEventAt;
  if (elapsed >= SPEED_STALE_MS) {
    return;
  }

  if (speedSamples.length < 2) {
    return;
  }

  const speed = estimateTransferSpeedBps();
  if (speed === null) {
    return;
  }

  if (speed <= 0) {
    showProgressSpeed("0 B/s");
    refreshLiveVisuals();
    return;
  }

  let speedText = `${formatBytes(speed)}/s`;
  const transferred = speedSamples[speedSamples.length - 1]?.bytes ?? 0;
  const total = progressTotalBytes;
  if (total > transferred && speed > 0) {
    const eta = formatEtaSeconds((total - transferred) / speed);
    if (eta) speedText += t(" · 约 {eta}", { eta });
  }
  showProgressSpeed(speedText);
  refreshLiveVisuals();
}

function refreshStallUi() {
  if (!isTransferring || lastProgressEventAt === 0) return;

  if (isConnecting) {
    applyActiveTransferBadge(isDownloadMode() ? "download" : "upload");
    showProgressSpeed(t("正在建立 SSH 连接…"));
    refreshLiveVisuals();
    return;
  }

  if (isChunkVerifying) {
    return;
  }

  const elapsed = Date.now() - lastProgressEventAt;
  if (elapsed < SPEED_STALE_MS && !isReconnecting) return;

  if (elapsed >= SPEED_STALE_MS) {
    showWaitingSpeedText(elapsed);
    refreshLiveVisuals();
  }

  if (elapsed >= UI_STALL_WARN_MS) {
    progressWrap.classList.add("stalled");
    applyActiveTransferBadge(isDownloadMode() ? "download" : "upload");
  }
}

function startProgressWatchdog() {
  stopProgressWatchdog();
  lastProgressEventAt = Date.now();
  progressWatchdogTimer = window.setInterval(refreshStallUi, SPEED_REFRESH_MS);
  speedDisplayTimer = window.setInterval(refreshSpeedDisplay, SPEED_REFRESH_MS);
}

function stopProgressWatchdog() {
  if (progressWatchdogTimer !== null) {
    window.clearInterval(progressWatchdogTimer);
    progressWatchdogTimer = null;
  }
  if (speedDisplayTimer !== null) {
    window.clearInterval(speedDisplayTimer);
    speedDisplayTimer = null;
  }
}

function updateDropZoneState() {
  const path = localPathInput.value.trim();
  // 路径被程序直接改写（复用历史、恢复断点、手动输入）时，退回单文件选择。
  if (selectedUploads[0] !== path) selectedUploads = path ? [path] : [];
  const hasPath = path.length > 0;
  dropZone.classList.toggle("has-file", hasPath);
  fileChip.classList.toggle("hidden", !hasPath);
  dropZone.classList.toggle("hidden", hasPath);
  if (!hasPath) return;

  if (isQueueSelection()) {
    fileChipName.textContent = t("{length} 个文件", { length: selectedUploads.length });
    fileChipPath.textContent = selectedUploads.map(fileNameOf).join("、");
    fileChipPath.title = selectedUploads.join("\n");
  } else {
    fileChipName.textContent = fileNameOf(path);
    fileChipPath.textContent = path;
    fileChipPath.title = path;
  }
}

function previewResolvedPath(): string | null {
  const remote = remotePathInput.value.trim();
  const local = localPathInput.value.trim();
  if (!remote || !local) return null;

  if (isDownloadMode()) {
    const remoteName = remote.split("/").pop() ?? "";
    if (!remoteName) return null;
    if (local.endsWith("/") || local.endsWith("\\")) {
      const base = local.replace(/[/\\]+$/, "");
      return `${base}/${remoteName}`;
    }
    const cp = activeDownloadCheckpoint;
    if (cp && cp.local_path !== local && cp.remote_path === remote) {
      return cp.local_path;
    }
    return null;
  }

  const fileName = local.split("/").pop() ?? "";
  if (!fileName) return null;

  if (remote.endsWith("/")) {
    return `${remote}${fileName}`;
  }

  if (
    activeUploadCheckpoint &&
    activeUploadCheckpoint.remote_path !== remote &&
    activeUploadCheckpoint.local_path === local
  ) {
    return activeUploadCheckpoint.remote_path;
  }

  return null;
}

function updateResolvedPathHint() {
  // 所有改动远端路径的入口（浏览选择、复用历史、恢复断点）都会走到这里，顺带同步收藏星标。
  updateBookmarkButton();
  if (isQueueSelection()) {
    const dir = remotePathInput.value.trim();
    resolvedPathHint.textContent = dir
      ? t("将依次上传 {count} 个文件到：{dir}", {
          count: selectedUploads.length,
          dir: dir.endsWith("/") ? dir : `${dir}/`,
        })
      : "";
    resolvedPathHint.classList.toggle("hidden", !dir);
    return;
  }
  const resolved = previewResolvedPath();
  const remote = remotePathInput.value.trim();
  if (resolved && resolved !== remote && resolved !== localPathInput.value.trim()) {
    const label = isDownloadMode() ? t("实际保存至") : t("实际上传至");
    resolvedPathHint.textContent = `${label}：${resolved}`;
    resolvedPathHint.classList.remove("hidden");
  } else {
    resolvedPathHint.classList.add("hidden");
  }
}

const HISTORY_STATUS_LABEL: Record<string, string> = {
  completed: t("完成"),
  failed: t("失败"),
  cancelled: t("已取消"),
};

function entryIcon(direction: TransferDirection): HTMLSpanElement {
  const el = document.createElement("span");
  el.className = `entry-icon${direction === "download" ? " is-download" : ""}`;
  el.innerHTML = direction === "download" ? icons.download : icons.upload;
  return el;
}

function entryMain(title: string, meta: string): HTMLDivElement {
  const main = document.createElement("div");
  main.className = "entry-main";
  const t = document.createElement("div");
  t.className = "entry-title";
  t.textContent = title;
  t.title = title;
  const m = document.createElement("div");
  m.className = "entry-meta";
  m.textContent = meta;
  m.title = meta;
  main.append(t, m);
  return main;
}

function makeButton(label: string, className: string, onClick: () => void): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = className;
  btn.textContent = label;
  btn.addEventListener("click", onClick);
  return btn;
}

/**
 * 未完成的传输单独列在历史页顶部：断点以前只藏在传输页的提示条里，
 * 用户关掉应用再打开时很难发现「还能接着传」。
 */
function renderPendingTransfers() {
  const items: { direction: TransferDirection; cp: UploadCheckpoint | DownloadCheckpoint }[] = [];
  // 正在传的那个方向不算「未完成」：它的断点就是当前任务本身，列出来只会让人以为有遗留任务。
  const running = isTransferring ? runDirection : null;
  if (activeUploadCheckpoint && running !== "upload") {
    items.push({ direction: "upload", cp: activeUploadCheckpoint });
  }
  if (activeDownloadCheckpoint && running !== "download") {
    items.push({ direction: "download", cp: activeDownloadCheckpoint });
  }

  navHistoryBadge.textContent = String(items.length);
  navHistoryBadge.classList.toggle("hidden", items.length === 0);
  pendingSection.classList.toggle("hidden", items.length === 0);

  pendingList.replaceChildren(
    ...items.map(({ direction, cp }) => {
      const row = document.createElement("div");
      row.className = "pending-item";
      const transferred = getTransferredBytes(cp);
      const name = fileNameOf(direction === "download" ? cp.remote_path : cp.local_path);
      const target = direction === "download" ? cp.local_path : `${cp.host}:${cp.remote_path}`;

      const progress = document.createElement("div");
      progress.className = "pending-progress";
      const track = document.createElement("div");
      track.className = "pending-track";
      const fill = document.createElement("div");
      fill.className = "pending-fill";
      fill.style.width = `${cp.file_size > 0 ? (transferred / cp.file_size) * 100 : 0}%`;
      track.append(fill);
      const label = document.createElement("span");
      label.textContent = `${formatBytes(transferred)} / ${formatBytes(cp.file_size)}`;
      progress.append(track, label);

      const actions = document.createElement("div");
      actions.className = "pending-actions";
      actions.append(
        makeButton(t("清除"), "btn btn-plain btn-small", () => void clearCheckpointFor(direction)),
        makeButton(t("继续"), "btn btn-primary btn-small", () => void resumePendingTransfer(direction)),
      );

      row.append(entryIcon(direction), entryMain(name, target), actions, progress);
      return row;
    }),
  );
}

function renderHistory() {
  if (historyEntries.length === 0) {
    const empty = document.createElement("div");
    empty.className = "list-empty";
    empty.textContent = t("还没有传输记录");
    historyList.replaceChildren(empty);
    return;
  }
  historyList.replaceChildren(
    ...historyEntries.map((entry) => {
      const direction: TransferDirection = entry.direction === "download" ? "download" : "upload";
      const row = document.createElement("div");
      row.className = "history-row";
      const name = fileNameOf(direction === "download" ? entry.remote_path : entry.local_path);
      const retries = entry.retry_count > 0 ? t(" · 重连 {retry_count} 次", { retry_count: entry.retry_count }) : "";
      const meta = `${entry.username}@${entry.host} · ${formatBytes(entry.file_size)} · ${formatTime(entry.finished_at)}${retries}`;
      const chip = document.createElement("span");
      chip.className = `status-chip ${entry.status}`;
      chip.textContent = HISTORY_STATUS_LABEL[entry.status] ?? entry.status;
      if (entry.message) chip.title = entry.message;
      row.append(
        entryIcon(direction),
        entryMain(name, meta),
        chip,
        makeButton(t("复用"), "btn btn-secondary btn-small", () => void reuseHistoryEntry(entry)),
      );
      return row;
    }),
  );
}

/** 按 host/port/user 找到已保存的服务器，用于带出凭据；找不到时只回填地址。 */
function findProfileFor(host: string, port: number, username: string): ConnectionProfile | undefined {
  return savedProfiles.find((p) => p.host === host && p.port === port && p.username === username);
}

function setTransferDirection(direction: TransferDirection) {
  if (transferDirection === direction) return;
  transferDirection = direction;
  const radio = document.querySelector<HTMLInputElement>(
    `input[name="transferDirection"][value="${direction}"]`,
  );
  if (radio) radio.checked = true;
  updateTransferDirectionUI();
}

async function reuseHistoryEntry(entry: UploadHistoryEntry) {
  if (isTransferring) return;
  setTransferDirection(entry.direction === "download" ? "download" : "upload");
  const profile = findProfileFor(entry.host, entry.port, entry.username);
  if (profile) {
    profileSelect.value = profile.id;
    await applyProfile(profile);
  } else {
    hostInput.value = entry.host;
    portInput.value = String(entry.port);
    usernameInput.value = entry.username;
    profileSelect.value = "";
    updateConnectionSummary();
  }
  localPathInput.value = entry.local_path;
  remotePathInput.value = entry.remote_path;
  updateDropZoneState();
  updateResolvedPathHint();
  clearProbeResult();
  showTransferView("compose");
  showPage("transfer");
  scheduleBackgroundProbe();
}

async function resumePendingTransfer(direction: TransferDirection) {
  if (isTransferring) return;
  setTransferDirection(direction);
  showTransferView("compose");
  showPage("transfer");
  // restore* 会把断点里的路径和服务器回填到表单，用户确认后点开始即可续传。
  if (direction === "download") await restoreDownloadCheckpoint();
  else await restoreUploadCheckpoint();
  transferBtn.focus();
}

async function clearCheckpointFor(direction: TransferDirection) {
  const download = direction === "download";
  const confirmed = await showConfirm(
    t("清除断点"),
    download
      ? t("确定清除下载断点记录？\n\n清除后下次下载将从头开始，本地已下载的部分不会自动删除。")
      : t("确定清除断点记录？\n\n清除后下次上传将从头开始，已传到远端的数据不会自动删除。"),
  );
  if (!confirmed) return;

  if (download) {
    await invoke("clear_saved_download_checkpoint");
    activeDownloadCheckpoint = null;
  } else {
    await invoke("clear_saved_checkpoint");
    activeUploadCheckpoint = null;
  }
  updateTransferStatePanel();
  setStatus(t("断点记录已清除"));
}

function defaultProfileName(): string {
  const alias = hostSelect.value.trim();
  if (alias) return alias;
  const user = usernameInput.value.trim();
  const host = hostInput.value.trim();
  if (user && host) return `${user}@${host}`;
  return host || user;
}

function setConnectionStatus(message: string, kind: "normal" | "error" | "ok" = "normal") {
  connectionStatus.textContent = message;
  connectionStatus.classList.remove("error", "ok");
  if (kind === "error") connectionStatus.classList.add("error");
  if (kind === "ok") connectionStatus.classList.add("ok");
}

async function applyProfile(profile: ConnectionProfile) {
  hostInput.value = profile.host;
  portInput.value = String(profile.port);
  usernameInput.value = profile.username;
  authTypeSelect.value = profile.auth_kind === "private_key" ? "key" : "password";
  if (profile.key_path) {
    keyPathInput.value = profile.key_path;
  } else if (profile.auth_kind === "private_key") {
    keyPathInput.value = "";
  }

  passwordInput.value = "";
  keyPassphraseInput.value = "";
  profileNameInput.value = profile.name;
  updateAuthFields();
  updateConnectionSummary();

  restorePostCommand();
  rememberSecretsInput.checked = profile.remember_secrets;
  updateAuthHintVisibility();
  if (!profile.remember_secrets) {
    const cached = sessionSecrets.get(profile.id);
    if (cached) {
      passwordInput.value = cached.password;
      keyPassphraseInput.value = cached.passphrase;
      updateConnectionSummary();
      setConnectionStatus(t("已加载「{name}」", { name: profile.name }), "ok");
      return;
    }
    setConnectionStatus(
      t("「{name}」设置为不保存密码，请输入密码后连接", { name: profile.name }),
      "normal",
    );
    return;
  }
  // 用户主动选择服务器时才读钥匙串：此时弹出系统授权框是可预期的。
  const secrets = await loadProfileSecrets(profile.id);
  if (profileSelect.value !== profile.id) return; // 读取期间用户已切到别的服务器
  passwordInput.value = secrets.password ?? "";
  keyPassphraseInput.value = secrets.key_passphrase ?? "";
  updateConnectionSummary();
  updateAuthHintVisibility();

  if (profile.auth_kind === "password") {
    setConnectionStatus(
      secrets.password
        ? t("已加载「{name}」，密码已自动填入", { name: profile.name })
        : t("已加载「{name}」，请填写密码后点保存", { name: profile.name }),
      secrets.password ? "ok" : "normal",
    );
  } else {
    setConnectionStatus(t("已加载「{name}」", { name: profile.name }), "ok");
  }
}

async function loadProfileSecrets(id: string): Promise<ProfileSecrets> {
  try {
    return await invoke<ProfileSecrets>("get_profile_secrets", { id });
  } catch {
    return { password: null, key_passphrase: null };
  }
}

/**
 * 连接前按需补齐所选服务器的密码/口令：启动时恢复断点不读钥匙串，
 * 拖到用户真正发起连接时才读，避免一打开应用就弹系统授权框。
 */
async function ensureCredentialsLoaded() {
  const id = profileSelect.value;
  // 设置为不记住密码的服务器，钥匙串里本来就没有，读取只会白白触发授权弹窗。
  if (!id || !profileRemembersSecrets()) return;
  const needsPassword = authTypeSelect.value === "password" && !passwordInput.value;
  const needsPassphrase = authTypeSelect.value === "key" && !keyPassphraseInput.value;
  if (!needsPassword && !needsPassphrase) return;
  const secrets = await loadProfileSecrets(id);
  if (needsPassword && secrets.password) passwordInput.value = secrets.password;
  if (needsPassphrase && secrets.key_passphrase) keyPassphraseInput.value = secrets.key_passphrase;
  updateConnectionSummary();
}

/** 所有会发起 SSH 连接的入口统一走这里：补凭据 → 校验表单 → 确认主机指纹。 */
async function prepareConnection() {
  await ensureCredentialsLoaded();
  validateConnectionBeforeTransfer();
  await ensureHostTrusted();
}

async function loadHistory() {
  historyEntries = await invoke<UploadHistoryEntry[]>("list_upload_history");
  renderHistory();
}

async function loadProfiles() {
  savedProfiles = await invoke<ConnectionProfile[]>("list_saved_profiles");
  profileSelect.replaceChildren();
  const defaultOption = document.createElement("option");
  defaultOption.value = "";
  defaultOption.textContent = t("未保存的服务器");
  profileSelect.appendChild(defaultOption);

  for (const profile of savedProfiles) {
    const option = document.createElement("option");
    option.value = profile.id;
    option.textContent = profile.name;
    profileSelect.appendChild(option);
  }
  renderServerList();
}

/** 服务器页左侧列表；选中项即「当前使用的服务器」，与传输页顶部的选择器保持一致。 */
function renderServerList() {
  navServerCount.textContent = String(savedProfiles.length);
  navServerCount.classList.toggle("hidden", savedProfiles.length === 0);
  if (savedProfiles.length === 0) {
    const empty = document.createElement("div");
    empty.className = "list-empty";
    empty.textContent = t("还没有保存的服务器\n在右侧填写后点「保存」");
    serverList.replaceChildren(empty);
    return;
  }
  serverList.replaceChildren(
    ...savedProfiles.map((profile) => {
      const item = document.createElement("button");
      item.type = "button";
      item.className = "server-item";
      item.dataset.id = profile.id;
      item.setAttribute("role", "option");
      const name = document.createElement("span");
      name.className = "server-item-name";
      name.textContent = profile.name;
      const meta = document.createElement("span");
      meta.className = "server-item-meta";
      meta.textContent = `${profile.username}@${profile.host}${profile.port === 22 ? "" : `:${profile.port}`}`;
      item.append(name, meta);
      item.disabled = isTransferring;
      item.addEventListener("click", () => {
        profileSelect.value = profile.id;
        void applyProfile(profile);
      });
      return item;
    }),
  );
  highlightSelectedServer();
}

function highlightSelectedServer() {
  serverList.querySelectorAll<HTMLButtonElement>(".server-item").forEach((item) => {
    const selected = item.dataset.id === profileSelect.value;
    item.classList.toggle("selected", selected);
    item.setAttribute("aria-selected", String(selected));
  });
}

/** 新建：清空编辑表单并取消选中，避免用户在已有服务器上改着改着覆盖了它。 */
function startNewServer() {
  profileSelect.value = "";
  profileNameInput.value = "";
  hostSelect.value = "";
  hostInput.value = "";
  portInput.value = "22";
  usernameInput.value = "";
  passwordInput.value = "";
  keyPassphraseInput.value = "";
  rememberSecretsInput.checked = true;
  setConnectionStatus("");
  updateAuthFields();
  updateConnectionSummary();
  profileNameInput.focus();
}

async function loadHosts() {
  const hosts = await invoke<SshHostEntry[]>("list_hosts");
  for (const host of hosts) {
    const option = document.createElement("option");
    option.value = host.alias;
    option.textContent = host.alias;
    hostSelect.appendChild(option);
  }
}

async function applyHostSelection(alias: string) {
  if (!alias) return;
  const entry = await invoke<SshHostEntry | null>("get_host", { alias });
  if (!entry) return;

  if (entry.hostname) hostInput.value = entry.hostname;
  if (entry.user) usernameInput.value = entry.user;
  if (entry.port) portInput.value = String(entry.port);
  if (entry.identity_file) {
    authTypeSelect.value = "key";
    keyPathInput.value = entry.identity_file;
    updateAuthFields();
  }
  profileNameInput.value = alias;
  updateConnectionSummary();
}

async function refreshCheckpointFromDisk() {
  activeUploadCheckpoint = await invoke<UploadCheckpoint | null>("get_saved_checkpoint");
  if (!activeUploadCheckpoint || activeUploadCheckpoint.status === "completed") {
    activeUploadCheckpoint = null;
  }
  activeDownloadCheckpoint = await invoke<DownloadCheckpoint | null>(
    "get_saved_download_checkpoint",
  );
  if (!activeDownloadCheckpoint || activeDownloadCheckpoint.status === "completed") {
    activeDownloadCheckpoint = null;
  }
  updateTransferStatePanel();
}

async function restoreUploadCheckpoint(options?: { keepStatus?: boolean }) {
  activeUploadCheckpoint = await invoke<UploadCheckpoint | null>("get_saved_checkpoint");
  if (!activeUploadCheckpoint || activeUploadCheckpoint.status === "completed") {
    activeUploadCheckpoint = null;
    if (!isDownloadMode()) updateTransferStatePanel();
    return;
  }

  if (isDownloadMode()) {
    updateTransferStatePanel();
    return;
  }

  const checkpoint = activeUploadCheckpoint;
  localPathInput.value = checkpoint.local_path;
  remotePathInput.value = checkpoint.remote_path;
  hostInput.value = checkpoint.host;
  portInput.value = String(checkpoint.port);
  usernameInput.value = checkpoint.username;
  authTypeSelect.value = checkpoint.auth_kind === "private_key" ? "key" : "password";
  if (checkpoint.key_path) keyPathInput.value = checkpoint.key_path;
  updateAuthFields();
  restoreCredentialsFromProfile(checkpoint.host, checkpoint.port, checkpoint.username);
  updateDropZoneState();
  updateResolvedPathHint();
  updateConnectionSummary();

  setProgressState("idle");

  // 断点状态由传输页顶部的状态面板统一呈现，页脚不再重复一遍。
  if (!options?.keepStatus) statusText.textContent = "";
  updateTransferStatePanel();
}

async function restoreDownloadCheckpoint() {
  activeDownloadCheckpoint = await invoke<DownloadCheckpoint | null>(
    "get_saved_download_checkpoint",
  );
  if (!activeDownloadCheckpoint || activeDownloadCheckpoint.status === "completed") {
    activeDownloadCheckpoint = null;
    if (isDownloadMode()) updateTransferStatePanel();
    return;
  }

  if (!isDownloadMode()) {
    updateTransferStatePanel();
    return;
  }

  const checkpoint = activeDownloadCheckpoint;
  localPathInput.value = checkpoint.local_path;
  remotePathInput.value = checkpoint.remote_path;
  hostInput.value = checkpoint.host;
  portInput.value = String(checkpoint.port);
  usernameInput.value = checkpoint.username;
  authTypeSelect.value = checkpoint.auth_kind === "private_key" ? "key" : "password";
  if (checkpoint.key_path) keyPathInput.value = checkpoint.key_path;
  updateAuthFields();
  restoreCredentialsFromProfile(checkpoint.host, checkpoint.port, checkpoint.username);
  updateDropZoneState();
  updateResolvedPathHint();
  updateConnectionSummary();

  setProgressState("idle");

  statusText.textContent = "";
  updateTransferStatePanel();
}

async function restoreActiveCheckpoint() {
  await restoreUploadCheckpoint();
  await restoreDownloadCheckpoint();
}

function setupDragDrop() {
  void getCurrentWebview().onDragDropEvent((event) => {
    // 拖入文件是整个窗口范围的：在别的页面拖进来也直接带到传输页，但传输中不打断。
    if (isDownloadMode() || isTransferring) return;
    if (event.payload.type === "over") {
      dropZone.classList.add("drag-over");
    } else if (event.payload.type === "leave") {
      dropZone.classList.remove("drag-over");
    } else if (event.payload.type === "drop") {
      dropZone.classList.remove("drag-over");
      const paths = event.payload.paths;
      if (paths.length > 0) {
        if (currentView !== "compose") showTransferView("compose");
        showPage("transfer");
        setSelectedUploads(paths);
      }
    }
  });
}

authTypeSelect.addEventListener("change", updateAuthFields);
rememberSecretsInput.addEventListener("change", updateAuthHintVisibility);

profileSelect.addEventListener("change", async () => {
  const id = profileSelect.value;
  if (!id) {
    updateConnectionSummary();
    return;
  }
  const profile = savedProfiles.find((p) => p.id === id);
  if (profile) await applyProfile(profile);
});

saveProfileBtn.addEventListener("click", async () => {
  const name = profileNameInput.value.trim() || defaultProfileName();
  if (!name) {
    setConnectionStatus(t("请先填写连接名称，或选择 SSH Config Host"), "error");
    profileNameInput.focus();
    return;
  }

  try {
    const saved = await invoke<ConnectionProfile>("save_connection_profile", {
      request: {
        name,
        host: hostInput.value.trim(),
        port: Number(portInput.value) || 22,
        username: usernameInput.value.trim(),
        auth_type: authTypeSelect.value,
        key_path: keyPathInput.value.trim() || null,
        password: passwordInput.value || null,
        key_passphrase: keyPassphraseInput.value || null,
        remember_secrets: rememberSecretsInput.checked,
      },
    });
    await loadProfiles();
    profileSelect.value = saved.id;
    profileNameInput.value = saved.name;
    rememberForSession();
    const stored = !rememberSecretsInput.checked
      ? t("，密码不保存（仅本次使用）")
      : authTypeSelect.value === "password" && passwordInput.value
        ? t("，密码已保存")
        : keyPassphraseInput.value
          ? t("，私钥口令已保存")
          : "";
    setConnectionStatus(t("已保存为「{name}」{stored}", { name: saved.name, stored }), "ok");
  } catch (error) {
    setConnectionStatus(describeError(error), "error");
  }
});

deleteProfileBtn.addEventListener("click", async () => {
  const id = profileSelect.value;
  if (!id) {
    setConnectionStatus(t("请先选择要删除的连接"), "error");
    return;
  }
  const confirmed = await showConfirm(
    t("删除连接"),
    t("确定删除这个已保存的连接？"),
  );
  if (!confirmed) return;

  try {
    await invoke("delete_connection_profile", { id });
    profileSelect.value = "";
    profileNameInput.value = "";
    await loadProfiles();
    setConnectionStatus(t("连接配置已删除"), "ok");
  } catch (error) {
    setConnectionStatus(describeError(error), "error");
  }
});

hostSelect.addEventListener("change", () => {
  void applyHostSelection(hostSelect.value);
});

[hostInput, usernameInput, portInput].forEach((input) => {
  input.addEventListener("input", () => {
    updateConnectionSummary();
    if (!profileNameInput.value.trim()) {
      profileNameInput.placeholder = defaultProfileName() || t("保存名称，如 myserver 或 user@host");
    }
  });
});

[passwordInput, keyPathInput, keyPassphraseInput].forEach((input) => {
  input.addEventListener("input", () => {
    rememberForSession();
    updateConnectionSummary();
    updateAuthHintVisibility();
  });
});

document.querySelectorAll('input[name="transferDirection"]').forEach((input) => {
  input.addEventListener("change", () => {
    const selected = document.querySelector<HTMLInputElement>(
      'input[name="transferDirection"]:checked',
    );
    transferDirection = (selected?.value as TransferDirection) ?? "upload";
    updateTransferDirectionUI();
    clearProbeResult();
    void restoreActiveCheckpoint();
  });
});

document.querySelectorAll('input[name="transferMode"]').forEach((input) => {
  input.addEventListener("change", updateModeHint);
});

confirmCancel.addEventListener("click", () => closeConfirm(false));
confirmOk.addEventListener("click", () => closeConfirm(true));
confirmOverlay.addEventListener("click", (event) => {
  if (event.target === confirmOverlay) closeConfirm(false);
});
document.addEventListener("keydown", (event) => {
  if (
    event.key === "Escape" &&
    confirmOverlay.classList.contains("is-visible") &&
    !confirmClosing
  ) {
    closeConfirm(false);
    return;
  }
  if (
    event.key === "Escape" &&
    remoteBrowseOverlay.classList.contains("is-visible")
  ) {
    closeRemoteBrowseOverlay();
  }
});

browseRemoteBtn.addEventListener("click", () => {
  void openRemoteBrowseDialog();
});

remoteBrowseCloseBtn.addEventListener("click", () => {
  closeRemoteBrowseOverlay();
});

remoteBrowseOverlay.addEventListener("click", (event) => {
  if (event.target === remoteBrowseOverlay) {
    closeRemoteBrowseOverlay();
  }
});

remoteBrowseGoBtn.addEventListener("click", () => {
  void loadRemoteBrowseDir(remoteBrowsePath.value);
});

remoteBrowsePath.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    void loadRemoteBrowseDir(remoteBrowsePath.value);
  }
});

remoteBrowseUpBtn.addEventListener("click", () => {
  if (remoteBrowseCurrent?.parent) {
    void loadRemoteBrowseDir(remoteBrowseCurrent.parent);
  }
});

remoteBrowseSelectDirBtn.addEventListener("click", () => {
  const path = remoteBrowseCurrent?.path ?? remoteBrowsePath.value.trim();
  if (!path) return;
  applyRemoteBrowseSelection(path, true);
});

remoteBrowseNewDirBtn.addEventListener("click", () => {
  showRemoteBrowseNewDirForm();
});

remoteBrowseCreateDirBtn.addEventListener("click", () => {
  void createRemoteBrowseDir();
});

remoteBrowseCancelNewDirBtn.addEventListener("click", () => {
  hideRemoteBrowseNewDirForm();
});

remoteBrowseNewDirName.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    void createRemoteBrowseDir();
  }
});

testConnBtn.addEventListener("click", async () => {
  try {
    testConnBtn.disabled = true;
    setConnectionStatus(t("正在测试连接…"));
    await prepareConnection();
    const message = await invoke<string>("test_connection", {
      request: connectionPayload(),
    });
    setConnectionStatus(message, "ok");
  } catch (error) {
    setConnectionStatus(clarifyTransferError(describeError(error)), "error");
  } finally {
    if (!isTransferring) testConnBtn.disabled = false;
  }
});

document.querySelector("#pickKeyBtn")!.addEventListener("click", async () => {
  const selected = await open({ multiple: false });
  if (typeof selected === "string") {
    keyPathInput.value = selected;
  }
});

/** 上传可多选（排队依次传）；下载选择的是本地保存目录，只能单选。 */
async function pickLocalPaths() {
  const download = isDownloadMode();
  const selected = await open({ multiple: !download, directory: download });
  if (selected === null) return;
  if (download) {
    const dir = selected as string;
    setSelectedUploads([dir.endsWith("/") ? dir : `${dir}/`]);
  } else {
    setSelectedUploads(Array.isArray(selected) ? selected : [selected]);
  }
}

document.querySelector("#pickFileBtn")!.addEventListener("click", () => void pickLocalPaths());
changeFileBtn.addEventListener("click", () => void pickLocalPaths());

forceOverwriteInput.addEventListener("change", () => {
  if (lastProbe) updateTransferStatePanel();
});

const languageHint = document.querySelector<HTMLParagraphElement>("#languageHint")!;

function initLanguagePreference() {
  const saved = languagePreference();
  const radio = document.querySelector<HTMLInputElement>(
    `input[name="languageMode"][value="${saved}"]`,
  );
  if (radio) radio.checked = true;
}

document.querySelectorAll<HTMLInputElement>('input[name="languageMode"]').forEach((radio) => {
  radio.addEventListener("change", () => {
    if (!radio.checked) return;
    // 切换语言会重新加载页面；传输虽在后端继续，但进行中视图会丢失本次的续传起点等信息。
    if (isTransferring || commandRunning) {
      initLanguagePreference();
      languageHint.textContent = t("传输进行中，完成后再切换语言");
      return;
    }
    setLanguagePreference(radio.value as LanguagePreference);
  });
});

document.querySelectorAll<HTMLInputElement>('input[name="themeMode"]').forEach((radio) => {
  radio.addEventListener("change", () => {
    if (!radio.checked) return;
    applyThemePreference(radio.value as ThemePreference);
  });
});

localPathInput.addEventListener("input", () => {
  updateDropZoneState();
  updateResolvedPathHint();
  invalidateProbeOnEdit();
});

localPathInput.addEventListener("blur", () => {
  if (!isTransferring) void runBackgroundProbe();
});

bookmarkBtn.addEventListener("click", () => void toggleBookmark());
saveCommandBtn.addEventListener("click", () => void toggleSavedCommand());
commandMenuBtn.addEventListener("click", () => {
  if (commandMenu.isOpen) commandMenu.close();
  else openCommandMenu();
});
postCommandInput.addEventListener("input", () => {
  localStorage.setItem(commandStorageKey(), postCommandInput.value);
  updateSaveCommandButton();
});
void listen<{ line: string }>("command-output", (event) => {
  if (commandRunning) appendConsoleLine(event.payload.line);
});
pathMenuBtn.addEventListener("click", () => {
  if (pathMenu.isOpen) pathMenu.close();
  else openPathMenu();
});

remotePathInput.addEventListener("input", () => {
  updateResolvedPathHint();
  invalidateProbeOnEdit();
});

remotePathInput.addEventListener("blur", () => {
  if (!isTransferring) void runBackgroundProbe();
});

/** 开始流程里有预检、确认框等多个 await，期间按钮仍可点；不加闸门的话双击会并发出第二个流程。 */
let startInFlight = false;

transferBtn.addEventListener("click", async () => {
  if (startInFlight || isTransferring) return;
  startInFlight = true;
  transferBtn.disabled = true;
  try {
    if (isDownloadMode()) {
      await startDownloadFlow();
    } else if (isQueueSelection()) {
      await startQueueFlow();
    } else {
      await startUploadFlow();
    }
  } catch (error) {
    setTransferring(false);
    // 后端拒绝启动时已经切到了进行中视图，退回编辑视图让用户看到错误并修改。
    if (currentView === "run") showTransferView("compose");
    setStatus(describeError(error), "error");
  } finally {
    startInFlight = false;
    // 用户在确认框里取消时流程正常返回、没有进入传输态，需要把按钮还回去。
    if (!isTransferring) transferBtn.disabled = false;
  }
});


resultBackBtn.addEventListener("click", () => {
  // 成功后「再传一个」：保留服务器和目标目录，只清掉上一个文件，方便连续传多个文件。
  if (resultCard.dataset.transferOk === "1") {
    if (isDownloadMode()) remotePathInput.value = remotePathInput.value.replace(/[^/]*$/, "");
    else localPathInput.value = "";
    updateDropZoneState();
    updateResolvedPathHint();
    clearProbeResult();
  }
  statusText.textContent = "";
  showTransferView("compose");
});

editServerBtn.addEventListener("click", () => showPage("servers"));
viewLogBtn.addEventListener("click", () => showPage("logs"));

document.querySelectorAll<HTMLInputElement>('input[name="logFilter"]').forEach((radio) => {
  radio.addEventListener("change", () => {
    if (radio.checked) activityLog.setFilter(radio.value as LogFilter);
  });
});

const copyLogBtn = document.querySelector<HTMLButtonElement>("#copyLogBtn")!;
copyLogBtn.addEventListener("click", async () => {
  await copyText(activityLog.toText());
  const label = copyLogBtn.lastChild;
  if (label) label.textContent = t("已复制");
  window.setTimeout(() => {
    if (label) label.textContent = t("复制");
  }, COPY_FEEDBACK_MS);
});

document.querySelector<HTMLButtonElement>("#clearLogBtn")!.addEventListener("click", () => {
  activityLog.clear();
  void invoke("clear_activity_log").catch(() => {});
});

// 先监听再补拉：补拉期间到达的实时日志由 ActivityLogView 按 id 去重。
void listen<ActivityEntry>("activity-log", (event) => activityLog.append(event.payload));
void invoke<ActivityEntry[]>("list_activity_log")
  .then((entries) => activityLog.load(entries))
  .catch(() => {
    // 浏览器预览模式下没有后端，日志页保持空状态。
  });
newServerBtn.addEventListener("click", startNewServer);
miniTransfer.addEventListener("click", () => showPage("transfer"));

document.querySelectorAll<HTMLButtonElement>(".nav-item").forEach((item) => {
  item.addEventListener("click", () => showPage(item.dataset.page as PageId));
});

/** 页面切换只改显隐，不重建 DOM：传输进行中切走再切回，进度与地图状态都保持原样。 */
function showPage(page: PageId) {
  currentPage = page;
  document.querySelectorAll<HTMLElement>(".page").forEach((el) => {
    el.classList.toggle("hidden", el.dataset.page !== page);
  });
  document.querySelectorAll<HTMLButtonElement>(".nav-item").forEach((el) => {
    const active = el.dataset.page === page;
    el.classList.toggle("active", active);
    if (active) el.setAttribute("aria-current", "page");
    else el.removeAttribute("aria-current");
  });
  activityLog.setVisible(page === "logs");
  // 迷你进度只在离开传输页时有意义；在传输页上它与大进度重复。
  miniTransfer.classList.toggle("hidden", !isTransferring || page === "transfer");
}

/** ⌘1–4 切页、⌘↩ 开始、⌘O 选文件：常用操作不必离开键盘。 */
document.addEventListener("keydown", (event) => {
  if (!event.metaKey || event.altKey || event.ctrlKey) return;
  if (confirmOverlay.classList.contains("is-visible") || remoteBrowseOverlay.classList.contains("is-visible")) return;
  const pageIndex = Number(event.key) - 1;
  if (Number.isInteger(pageIndex) && pageIndex >= 0 && pageIndex < PAGE_ORDER.length) {
    event.preventDefault();
    showPage(PAGE_ORDER[pageIndex]);
    return;
  }
  if (event.key === "Enter" && currentPage === "transfer" && currentView !== "run") {
    event.preventDefault();
    transferBtn.click();
    return;
  }
  if (event.key.toLowerCase() === "o" && currentPage === "transfer" && currentView === "compose") {
    event.preventDefault();
    document.querySelector<HTMLButtonElement>("#pickFileBtn")!.click();
  }
});

cancelBtn.addEventListener("click", () => {
  if (commandRunning) {
    cancelBtn.disabled = true;
    cancelBtn.textContent = t("停止中…");
    void invoke("cancel_remote_command").catch(() => {});
    return;
  }
  if (cancelInFlight || !isTransferring) return;

  cancelInFlight = true;
  cancelBtn.disabled = true;
  cancelBtn.textContent = t("取消中…");
  cancelBtn.classList.add("is-cancelling");
  setBadge(t("取消中"), "retrying");
  setStatus(t("正在取消…"));

  const cmd = runDirection === "download" ? "cancel_download" : "cancel_upload";
  void invoke(cmd).catch((error) => {
    cancelInFlight = false;
    if (isTransferring) {
      cancelBtn.disabled = false;
      cancelBtn.textContent = t("取消传输");
      cancelBtn.classList.remove("is-cancelling");
    }
    setStatus(describeError(error), "error");
  });
});

clearCheckpointBtn.addEventListener("click", () => {
  void clearCheckpointFor(transferDirection);
});


/** 「已复制」反馈停留时长。 */
const COPY_FEEDBACK_MS = 1500;

/** 完成后留给分块地图播放绿色波纹的时间，略长于 BlockMap 的波纹时长。 */
const COMPLETE_REVEAL_MS = 1100;

type ResultKind = "ok" | "warn" | "error" | "paused";

interface RunStats {
  elapsedMs: number;
  movedBytes: number;
  retries: number;
}

function prefersReducedMotion(): boolean {
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/** 必须在 setTransferring(false) / resetSpeedTracker 之前取，之后计数会被清零。 */
function collectRunStats(): RunStats {
  return {
    elapsedMs: runStartedAt > 0 ? Date.now() - runStartedAt : 0,
    movedBytes: Math.max(0, progressTransferredBytes - (runStartBytes ?? progressTransferredBytes)),
    retries: Math.max(retryCount, blockMap.retryCount),
  };
}

function formatDuration(ms: number): string {
  const secs = Math.round(ms / 1000);
  if (secs < 60) return t("{secs} 秒", { secs });
  const mins = Math.floor(secs / 60);
  if (mins < 60) return t("{mins} 分 {p2} 秒", { mins, p2: secs % 60 });
  return t("{p1} 小时 {p2} 分", { p1: Math.floor(mins / 60), p2: mins % 60 });
}

/** 控制台只保留最近若干行，长时间输出的命令也不会让 DOM 无限增长。 */
const CONSOLE_MAX_LINES = 200;
const COMMAND_STORAGE_PREFIX = "resiliss-post-command:";
const COMMAND_VARIABLES_HINT = "{remote_dir} {file_name} {remote_path} {files}";

/** 按服务器记住上次填写的命令：不同服务器的部署命令通常不同。 */
function commandStorageKey(): string {
  return COMMAND_STORAGE_PREFIX + (profileSelect.value || `${hostInput.value.trim()}:${portInput.value}`);
}

function restorePostCommand() {
  postCommandInput.value = localStorage.getItem(commandStorageKey()) ?? "";
  updateSaveCommandButton();
}

function updateSaveCommandButton() {
  const profile = currentProfile();
  const command = postCommandInput.value.trim();
  const saved = Boolean(profile && command && profile.post_commands.includes(command));
  saveCommandBtn.disabled = !profile || !command || isTransferring;
  saveCommandBtn.classList.toggle("is-active", saved);
  saveCommandBtn.innerHTML = saved ? icons.starFilled : icons.star;
  saveCommandBtn.title = !profile
    ? t("保存服务器后才能保存常用命令")
    : saved
      ? t("从常用命令中移除")
      : t("保存为常用命令");
}

async function toggleSavedCommand() {
  const profile = currentProfile();
  const command = postCommandInput.value.trim();
  if (!profile || !command) return;
  const commands = profile.post_commands.includes(command)
    ? profile.post_commands.filter((c) => c !== command)
    : [command, ...profile.post_commands];
  try {
    await saveProfileExtras(profile, { post_commands: commands });
    updateSaveCommandButton();
  } catch (error) {
    setStatus(describeError(error), "error");
  }
}

function openCommandMenu() {
  const profile = currentProfile();
  const commands = profile?.post_commands ?? [];
  const footer = document.createElement("div");
  footer.append(t("可用变量（自动加引号）："));
  const vars = document.createElement("code");
  vars.textContent = COMMAND_VARIABLES_HINT;
  footer.append(vars);
  commandMenu.show(
    [
      {
        items: commands.map((command) => ({
          label: command,
          onSelect: () => {
            postCommandInput.value = command;
            postCommandInput.dispatchEvent(new Event("input"));
          },
          onRemove: () => {
            if (!profile) return;
            void saveProfileExtras(profile, {
              post_commands: commands.filter((c) => c !== command),
            }).then(() => {
              updateSaveCommandButton();
              openCommandMenu();
            });
          },
        })),
      },
    ],
    profile ? t("还没有常用命令\n填好命令后点输入框里的 ☆ 保存") : t("保存服务器后即可保存常用命令"),
    footer,
  );
}

function commandContextFor(remotePaths: string[]): CommandContext {
  const last = remotePaths[remotePaths.length - 1] ?? "";
  return {
    remote_dir: remoteDirOf(last),
    file_name: fileNameOf(last),
    remote_path: last,
    files: remotePaths.map(fileNameOf),
  };
}

function appendConsoleLine(line: string) {
  commandOutput.append(`${line}\n`);
  while (commandOutput.childNodes.length > CONSOLE_MAX_LINES) commandOutput.firstChild?.remove();
  commandOutput.scrollTop = commandOutput.scrollHeight;
}

/**
 * 传输全部成功后执行命令。期间留在进行中视图：地图保持完成态，下方控制台实时显示输出，
 * 「取消」按钮改为停止命令。返回 null 表示命令根本没跑起来（连接失败等），错误文本放在 error 里。
 */
async function runPostCommand(
  template: string,
  context: CommandContext,
): Promise<{ outcome: CommandOutcome | null; error?: string }> {
  commandRunning = true;
  commandOutput.textContent = "";
  commandConsoleCmd.textContent = template;
  commandConsole.classList.remove("hidden");
  setBadge(t("执行命令中"), "verifying");
  cancelBtn.textContent = t("停止命令");
  cancelBtn.disabled = false;
  cancelBtn.classList.remove("hidden", "is-cancelling");
  navTransferLive.classList.remove("hidden");
  try {
    const outcome = await invoke<CommandOutcome>("run_remote_command", {
      request: { connection: connectionPayload(), template, context },
    });
    return { outcome };
  } catch (error) {
    return { outcome: null, error: describeError(error) };
  } finally {
    commandRunning = false;
    commandConsole.classList.add("hidden");
    cancelBtn.classList.add("hidden");
    navTransferLive.classList.add("hidden");
  }
}

/** 结果页的命令区：成功也展示末尾输出，方便确认服务确实起来了。 */
function showCommandResult(outcome: CommandOutcome | null, error?: string) {
  if (!outcome && !error) {
    resultCommand.classList.add("hidden");
    return;
  }
  resultCommand.classList.remove("hidden");
  resultCommandLine.textContent = `$ ${outcome?.command ?? runPostCommandTemplate}`;
  const output = outcome ? outcome.output_tail.join("\n") : (error ?? "");
  resultCommandOutput.textContent = output || t("（没有输出）");
}

function describeCommandFailure(outcome: CommandOutcome | null, error?: string): string {
  if (!outcome) return t("命令没能执行：{p1}", { p1: error ?? t("未知错误") });
  if (outcome.cancelled) return t("命令已被停止（远端进程可能仍在运行）");
  if (outcome.timed_out) return t("命令超过 10 分钟没有任何输出，已停止等待");
  return t("命令执行失败，退出码 {p1}", { p1: outcome.exit_code ?? t("未知") });
}

/** 传输成功后的收尾：有命令就先执行命令，再按命令结果决定结果页是「完成」还是「警告」。 */
async function finishSuccessfulRun(
  title: string,
  message: string,
  stats: RunStats,
  remotePaths: string[],
) {
  if (!runPostCommandTemplate) {
    showCommandResult(null);
    showResult("ok", title, message, stats);
    return;
  }
  const { outcome, error } = await runPostCommand(runPostCommandTemplate, commandContextFor(remotePaths));
  showCommandResult(outcome, error);
  if (outcome?.exit_code === 0 && !outcome.cancelled && !outcome.timed_out) {
    showResult("ok", title, t("{message}\n已执行命令，退出码 0", { message }), stats);
  } else {
    showResult("warn", t("{title}，但命令未成功", { title }), describeCommandFailure(outcome, error), stats);
  }
}

/**
 * transferOk：本次的文件是否全部传输成功。「传完但命令失败」仍为 true（下一步是再传一个），
 * 「批量中部分文件失败」为 false（下一步是重试失败的文件）。两者都显示为警告，但后续操作不同。
 */
function showResult(
  kind: ResultKind,
  title: string,
  subtitle: string,
  stats: RunStats,
  transferOk = kind === "ok" || kind === "warn",
) {
  resultCard.className = `result-card ${kind}`;
  resultCard.dataset.transferOk = transferOk ? "1" : "";
  resultIcon.innerHTML =
    kind === "ok" ? icons.check : kind === "paused" ? icons.pause : icons.alert;
  resultTitle.textContent = title;
  resultSubtitle.textContent = subtitle;
  // 「传完但命令失败」时文件本身已校验通过，校验结论仍然有效，要保留。
  if (kind !== "ok" && kind !== "warn") hideVerifySummary();
  if (kind === "error" || kind === "paused") resultCommand.classList.add("hidden");

  const avg = stats.elapsedMs > 0 ? stats.movedBytes / (stats.elapsedMs / 1000) : 0;
  const rows: [string, string][] = [
    [t("用时"), formatDuration(stats.elapsedMs)],
    [t("本次传输"), formatBytes(stats.movedBytes)],
    [stats.retries > 0 ? t("自动重连") : t("平均速度"), stats.retries > 0 ? t("{retries} 次", { retries: stats.retries }) : `${formatBytes(avg)}/s`],
  ];
  // 没有实际传输（如远端已是最新）时，用时、速度都没有意义。
  resultStats.classList.toggle("hidden", stats.movedBytes === 0 && stats.retries === 0);
  resultStats.replaceChildren(
    ...rows.map(([label, value]) => {
      const item = document.createElement("div");
      const dt = document.createElement("dt");
      dt.textContent = label;
      const dd = document.createElement("dd");
      dd.textContent = value;
      item.append(dt, dd);
      return item;
    }),
  );
  showTransferView("result");
}

function handleProgressUpdate(
  transferred: number,
  total: number,
  status: string,
  message: string,
  retry_count: number,
  direction: TransferDirection,
  verify_summary?: string | null,
) {
  retryCount = retry_count;

  if (
    status !== "completed" &&
    status !== "cancelled" &&
    status !== "failed" &&
    status !== "cancelling"
  ) {
    isConnecting = false;
  }

  const uploading = direction === "upload";

  if (status === "stalled") {
    isReconnecting = true;
    progressWrap.classList.add("stalled");
    applyActiveTransferBadge(direction);
    showWaitingSpeedText(
      lastProgressEventAt > 0 ? Date.now() - lastProgressEventAt : UI_STALL_WARN_MS,
    );
    updateProgressDetail(transferred, total, retry_count);
    showTransferStatus(message, "retry");
    refreshLiveVisuals();
    return;
  }

  if (status === "completed") {
    updateProgress(transferred, total);
    const stats = collectRunStats();
    setTransferring(false);
    setProgressState("idle");
    resetSpeedTracker();
    setBadge(t("已完成"), "verifying");
    statusText.textContent = "";
    cancelBtn.classList.add("hidden");
    blockMap.setPhase("completed");
    showVerifySummary(
      verify_summary ??
        (isReliableMode()
          ? t("校验通过 · 逐块校验 + SHA-256 一致")
          : t("校验通过 · SHA-256 一致")),
    );
    if (uploading) activeUploadCheckpoint = null;
    else activeDownloadCheckpoint = null;
    updateTransferStatePanel();
    void loadHistory();
    // 队列中的单个文件完成：交回调度器继续下一个，不展示单文件结果页。
    if (resolveQueueStep) {
      resolveQueueStep({ status: "completed", message, stats });
      return;
    }
    // 先让地图播完「全部变绿」的波纹再切结果页：这一刻是整个等待的回报，值得被看到。
    window.setTimeout(() => {
      if (isTransferring) return;
      void finishSuccessfulRun(uploading ? t("上传完成") : t("下载完成"), message, stats, [runRemotePath]);
    }, prefersReducedMotion() ? 0 : COMPLETE_REVEAL_MS);
    return;
  }

  if (status === "cancelled") {
    const stats = collectRunStats();
    setTransferring(false);
    setProgressState("idle");
    resetSpeedTracker();
    hideVerifySummary();
    blockMap.setPhase("cancelled");
    statusText.textContent = "";
    void restoreActiveCheckpoint();
    void loadHistory();
    if (resolveQueueStep) {
      resolveQueueStep({ status: "cancelled", message, stats });
      return;
    }
    showResult(
      "paused",
      t("已暂停"),
      t("已传输的部分保存为断点，随时可以从这里继续。"),
      stats,
    );
    return;
  }

  if (status === "cancelling") {
    cancelBtn.disabled = true;
    cancelBtn.textContent = t("取消中…");
    cancelBtn.classList.add("is-cancelling");
    setProgressState("retrying");
    setBadge(t("取消中"), "retrying");
    showTransferStatus(message, "retry");
    return;
  }

  if (status === "failed") {
    const stats = collectRunStats();
    setTransferring(false);
    setProgressState("idle");
    resetSpeedTracker();
    hideVerifySummary();
    blockMap.setPhase("failed");
    statusText.textContent = "";
    void refreshCheckpointFromDisk();
    void loadHistory();
    if (resolveQueueStep) {
      resolveQueueStep({ status: "failed", message, stats });
      return;
    }
    showResult("error", uploading ? t("上传失败") : t("下载失败"), clarifyTransferError(message), stats);
    return;
  }

  if (status === "verifying") {
    isChunkVerifying = false;
    isFullVerifying = true;
    markProgressActivity();
    setProgressState("verifying");
    setBadge(t("校验中"), "verifying");
    updateProgress(transferred, total);
    updateProgressDetail(transferred, total, retry_count);
    showTransferStatus(t("正在校验文件完整性，请稍候…"));
    showProgressSpeed(t("校验中"));
    refreshLiveVisuals();
    return;
  }

  if (status === "chunk_verifying") {
    // 块读回校验对用户透明，外观与正常传输一致，仅刷新心跳避免误报 stall
    isChunkVerifying = true;
    markProgressActivity();
    applyActiveTransferBadge(direction);
    updateProgress(transferred, total);
    updateProgressDetail(transferred, total, retry_count);
    hideTransferStatus();
    refreshLiveVisuals();
    return;
  }

  isChunkVerifying = false;
  isFullVerifying = false;

  if (status === "retrying") {
    isReconnecting = true;
    setProgressState("retrying");
    setBadge(t("重试中"), "retrying");
    showReconnectingSpeedText();
    updateProgressDetail(transferred, total, retry_count);
    showTransferStatus(message, "retry");
    refreshLiveVisuals();
    return;
  }

  applyActiveTransferBadge(direction);
  recordSpeedSample(transferred);
  updateProgress(transferred, total);
  updateProgressDetail(transferred, total, retry_count);
  refreshSpeedDisplay();
  hideTransferStatus();
  refreshLiveVisuals();
}

void listen("transfer-close-requested", async () => {
  const ok = await showConfirm(
    t("传输进行中"),
    t("关闭窗口将取消当前传输，已传输的部分会保留为断点。\n\n确定关闭吗？"),
  );
  if (!ok) return;

  await invoke("prepare_app_close");
  await getCurrentWindow().close();
});

/**
 * 启动顺序有依赖：恢复断点时要按 host/user 匹配已保存的服务器，所以先加载服务器列表；
 * 是否提示继续传输，又要等确认后端当前没有正在进行的任务之后才能决定。
 */
async function startupSequence() {
  const settle = (task: Promise<unknown>) => task.catch(() => {});
  await settle(loadProfiles());
  await settle(restoreActiveCheckpoint());
  await settle(syncTransferStateOnStartup());
  await offerResumeOnLaunch();
}

/**
 * 启动时发现未完成的传输就主动询问是否继续：应用崩溃或电脑重启后，
 * 用户往往不记得还有传了一半的文件，等他去历史页翻就晚了。
 */
async function offerResumeOnLaunch() {
  if (isTransferring) return;
  const pending: [TransferDirection, UploadCheckpoint | DownloadCheckpoint][] = [];
  if (activeUploadCheckpoint) pending.push(["upload", activeUploadCheckpoint]);
  if (activeDownloadCheckpoint) pending.push(["download", activeDownloadCheckpoint]);
  if (pending.length === 0) return;

  const [direction, cp] = pending[0];
  const transferred = getTransferredBytes(cp);
  const name = fileNameOf(direction === "download" ? cp.remote_path : cp.local_path);
  const percent = cp.file_size > 0 ? Math.floor((transferred / cp.file_size) * 100) : 0;
  const more = pending.length > 1 ? t("\n\n另有 1 个未完成的传输，可在「历史」页查看。") : "";
  const resume = await showConfirm(
    t("有未完成的传输"),
    t("「{name}」已{verb} {percent}%（{done} / {total}），要现在继续吗？", {
      name,
      verb: direction === "download" ? t("下载") : t("上传"),
      percent,
      done: formatBytes(transferred),
      total: formatBytes(cp.file_size),
    }) + more,
    { ok: t("继续传输"), cancel: t("稍后") },
  );
  if (!resume) return;
  await resumePendingTransfer(direction);
  transferBtn.click();
}

async function syncTransferStateOnStartup() {
  try {
    const state = await invoke<{ upload_running: boolean; download_running: boolean }>(
      "get_transfer_running",
    );
    if (state.upload_running || state.download_running) {
      transferDirection = state.download_running ? "download" : "upload";
      const radio = document.querySelector<HTMLInputElement>(
        `input[name="transferDirection"][value="${transferDirection}"]`,
      );
      if (radio) radio.checked = true;
      updateTransferDirectionUI();
      setTransferring(true);
      // 重新接上已在进行的传输：真实起点未知，不能把此刻的进度画成「之前已续传」。
      blockMap.reset(0, 0);
      syncMapLegend();
      setProgressState("uploading");
      setBadge(state.download_running ? t("下载中") : t("上传中"), "uploading");
      setStatus(t("检测到进行中的传输任务…"));
    }
  } catch {
    // 忽略启动同步失败
  }
}

void listen<UploadProgressEvent>("upload-progress", (event) => {
  const payload = event.payload;
  handleProgressUpdate(
    payload.uploaded_bytes,
    payload.total_bytes,
    payload.status,
    payload.message,
    payload.retry_count,
    "upload",
    payload.verify_summary,
  );
});

void listen<DownloadProgressEvent>("download-progress", (event) => {
  const payload = event.payload;
  handleProgressUpdate(
    payload.downloaded_bytes,
    payload.total_bytes,
    payload.status,
    payload.message,
    payload.retry_count,
    "download",
    payload.verify_summary,
  );
});

void listen<UploadRetryEvent>("upload-retry", (event) => {
  retryCount = event.payload.retry_count;
  retryDelayMs = event.payload.delay_ms;
  retryScheduledAt = Date.now();
  isReconnecting = true;
  isConnecting = false;
  blockMap.markRetry();
  syncMapLegend();
  updateProgressDetail(progressTransferredBytes, progressTotalBytes, retryCount);
  setProgressState("retrying");
  setBadge(t("重试中"), "retrying");
  showReconnectingSpeedText();
  showTransferStatus(event.payload.message, "retry");
  refreshLiveVisuals();
});

void listen<DownloadRetryEvent>("download-retry", (event) => {
  retryCount = event.payload.retry_count;
  retryDelayMs = event.payload.delay_ms;
  retryScheduledAt = Date.now();
  isReconnecting = true;
  isConnecting = false;
  blockMap.markRetry();
  syncMapLegend();
  updateProgressDetail(progressTransferredBytes, progressTotalBytes, retryCount);
  setProgressState("retrying");
  setBadge(t("重试中"), "retrying");
  showReconnectingSpeedText();
  showTransferStatus(event.payload.message, "retry");
  refreshLiveVisuals();
});

setupDragDrop();
initThemePreference();
initLanguagePreference();
showTransferView("compose");
updateConnectionSummary();
updateModeHint();
updateTransferDirectionUI();
void loadHosts();
void loadHistory();
void startupSequence();
void getVersion()
  .then((version) => {
    appVersion.textContent = `v${version}`;
  })
  .catch(() => {
    // 浏览器预览模式下拿不到版本号，留空即可。
  });
updateAuthFields();
updateDropZoneState();
updateResolvedPathHint();
updateAuthHintVisibility();
updateRemoteBrowseModeUi();
