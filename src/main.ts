import "./theme.css";
import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";

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
  action: "new" | "resume" | "overwrite" | "full_reupload" | "verify_retry";
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

(function initThemeEarly() {
  const saved = localStorage.getItem(THEME_STORAGE_KEY) as ThemePreference | null;
  if (saved === "light" || saved === "dark") {
    document.documentElement.dataset.theme = saved;
  }
})();

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

app.innerHTML = `
  <div class="main-scroll">
    <div class="card" id="connectionCard">
      <button class="collapsible-header" id="connectionToggle" type="button">
        <h2>连接设置</h2>
        <span class="connection-summary" id="connectionSummary">点击展开配置</span>
        <span class="chevron">▼</span>
      </button>
      <div id="connectionBody" class="collapsible-body collapsed">
        <div class="connection-body-inner">
          <section class="connection-section">
            <label for="profileSelect">快速连接</label>
            <select id="profileSelect">
              <option value="">选择已保存连接</option>
            </select>
            <div id="connectionStatus" class="connection-status"></div>
            <div class="save-name-row">
              <input id="profileName" placeholder="连接名称，如 甘肃服务器" />
              <button class="secondary small" id="saveProfileBtn" type="button">保存</button>
              <button class="ghost danger-text small" id="deleteProfileBtn" type="button">删除</button>
            </div>
            <p class="hint auth-hint" id="authHint">密码随已保存连接一并存储，选好连接后自动填入</p>
          </section>

          <div class="connection-divider" aria-hidden="true"><span>或手动配置</span></div>

          <section class="connection-section">
            <label for="hostSelect">从 SSH Config 导入 <span class="label-hint">~/.ssh/config</span></label>
            <select id="hostSelect">
              <option value="">不使用，手动填写下方信息</option>
            </select>

            <div class="row">
              <div>
                <label for="host">主机</label>
                <input id="host" placeholder="192.168.1.10" />
              </div>
              <div>
                <label for="port">端口</label>
                <input id="port" type="number" value="22" />
              </div>
            </div>

            <label for="username">用户名</label>
            <input id="username" placeholder="root" />

            <label for="authType">认证方式</label>
            <select id="authType">
              <option value="key">私钥</option>
              <option value="password">密码</option>
            </select>

            <div id="keyFields">
              <label for="keyPath">私钥路径</label>
              <div class="file-row">
                <input id="keyPath" placeholder="~/.ssh/id_rsa" />
                <button class="secondary" id="pickKeyBtn" type="button">选择</button>
              </div>
              <label for="keyPassphrase">私钥口令（可选）</label>
              <input id="keyPassphrase" type="password" />
            </div>

            <div id="passwordFields" class="hidden">
              <label for="password">密码</label>
              <input id="password" type="password" placeholder="密码登录时填写" />
            </div>

            <div class="connection-actions">
              <button class="secondary small" id="testConnBtn" type="button">测试连接</button>
            </div>
          </section>
        </div>
      </div>
    </div>

    <div class="card" id="fileCard">
      <div class="card-brand-row">
        <div class="app-brand-group">
          <img class="app-logo" src="/app-logo.png" alt="" width="32" height="32" />
          <span class="app-brand">${APP_NAME}</span>
        </div>
        <div class="transfer-direction compact">
          <div class="mode-toggle direction-toggle">
            <label class="radio-pill direction-pill">
              <input type="radio" name="transferDirection" value="upload" checked />
              <span>上传</span>
            </label>
            <label class="radio-pill direction-pill">
              <input type="radio" name="transferDirection" value="download" />
              <span>下载</span>
            </label>
          </div>
        </div>
      </div>

      <div id="transferStatePanel" class="state-panel hidden">
        <div class="state-panel-main">
          <div class="state-panel-title" id="statePanelTitle"></div>
          <div class="state-panel-detail" id="statePanelDetail"></div>
          <div class="state-panel-hint" id="statePanelHint"></div>
        </div>
        <button class="ghost-link hidden" id="clearCheckpointBtn" type="button">清除断点</button>
      </div>

      <label for="localPath" id="localPathLabel">本地文件</label>
      <div id="fileChip" class="file-chip hidden">
        <div class="file-chip-icon">📄</div>
        <div class="file-chip-body">
          <div class="file-chip-name" id="fileChipName"></div>
          <div class="file-chip-path" id="fileChipPath"></div>
        </div>
        <button class="secondary small" id="changeFileBtn" type="button">更换</button>
      </div>
      <div class="drop-zone" id="dropZone">
        <div class="drop-icon">📄</div>
        <div class="file-row">
          <input id="localPath" placeholder="选择或拖拽文件到此处" />
          <button class="secondary" id="pickFileBtn" type="button">选择</button>
        </div>
        <p class="drop-hint" id="dropHint">支持将文件拖拽到此处</p>
      </div>

      <label for="remotePath" id="remotePathLabel">远端路径</label>
      <div class="file-row remote-path-row">
        <input id="remotePath" placeholder="/home/user/dir/ 或 /home/user/file.jar" />
        <button class="secondary" id="browseRemoteBtn" type="button">浏览</button>
      </div>
      <p id="resolvedPathHint" class="resolved-path hidden"></p>
      <p class="hint path-hint" id="pathHint">填目录时以 / 结尾；若远端已是同名目录，也会自动拼接文件名</p>

      <div class="transfer-settings-card">
        <button class="collapsible-header" id="transferSettingsToggle" type="button">
          <span class="transfer-settings-title">传输选项</span>
          <span class="transfer-settings-summary" id="transferSettingsSummary">弱网可靠</span>
          <span class="chevron">▼</span>
        </button>
        <div id="transferSettingsBody" class="collapsible-body collapsed">
          <div class="transfer-settings-inner">
            <div class="transfer-settings-head">
              <span class="transfer-settings-label">传输模式</span>
              <div class="mode-toggle mode-toggle-subtle">
                <label class="radio-pill mode-pill">
                  <input type="radio" name="transferMode" value="reliable" checked />
                  <span>弱网可靠</span>
                </label>
                <label class="radio-pill mode-pill">
                  <input type="radio" name="transferMode" value="fast" />
                  <span>快速传输</span>
                </label>
              </div>
            </div>
            <p class="hint mode-hint" id="modeHint">弱网下自动逐块校验，速度略慢但更可靠</p>
            <label class="checkbox-row">
              <input type="checkbox" id="forceOverwrite" />
              <span id="forceOverwriteLabel">强制从头覆盖（忽略断点与远端已有数据）</span>
            </label>
            <div class="transfer-settings-head theme-settings-head">
              <span class="transfer-settings-label">外观</span>
              <div class="mode-toggle mode-toggle-subtle">
                <label class="radio-pill mode-pill">
                  <input type="radio" name="themeMode" value="system" checked />
                  <span>系统</span>
                </label>
                <label class="radio-pill mode-pill">
                  <input type="radio" name="themeMode" value="light" />
                  <span>浅色</span>
                </label>
                <label class="radio-pill mode-pill">
                  <input type="radio" name="themeMode" value="dark" />
                  <span>深色</span>
                </label>
              </div>
            </div>
          </div>
        </div>
      </div>

      <div class="transfer-actions">
        <button class="primary" id="transferBtn" type="button">开始上传</button>
      </div>

      <div class="progress-wrap" id="progressWrap">
        <div class="progress-toolbar" id="progressToolbar">
          <span id="progressBadge" class="badge">等待开始</span>
          <button class="cancel-btn hidden" id="cancelBtn" type="button">取消</button>
        </div>
        <progress id="progressBar" value="0" max="100"></progress>
        <div class="progress-stats hidden" id="progressStats">
          <div class="progress-detail hidden" id="progressDetail"></div>
          <div class="progress-speed-line hidden" id="progressSpeed"></div>
        </div>
        <div class="verify-summary hidden" id="verifySummary"></div>
        <div class="ambient-panel hidden" id="transferActivity" aria-hidden="true">
          <div class="data-flow mood-steady" id="transferScene" aria-hidden="true">
            <div class="data-flow-pipe">
              <div class="data-flow-pipe-track"></div>
              <span class="data-flow-node data-flow-local" aria-hidden="true"></span>
              <span class="data-flow-node data-flow-remote" aria-hidden="true"></span>
              <div class="data-flow-particles" aria-hidden="true">
                <span class="data-packet"></span>
                <span class="data-packet"></span>
                <span class="data-packet"></span>
                <span class="data-packet"></span>
                <span class="data-packet"></span>
              </div>
              <!-- 传输小信使：随网速切换表情（🚀快 / ✈️正常 / 🐌慢 / 😴等待），给等待的用户解闷 -->
              <span class="data-flow-courier" aria-hidden="true"></span>
            </div>
          </div>
        </div>
        <div class="status" id="statusText">填写连接信息和文件后开始传输</div>
        <div class="history-peek hidden" id="historyPeek"></div>
      </div>
    </div>
  </div>

  <div id="confirmOverlay" class="modal-overlay hidden">
    <div class="modal" role="dialog" aria-modal="true">
      <div class="modal-title" id="confirmTitle">确认</div>
      <div class="modal-body" id="confirmBody"></div>
      <div class="modal-actions">
        <button class="secondary" id="confirmCancel" type="button">取消</button>
        <button class="primary" id="confirmOk" type="button">确定</button>
      </div>
    </div>
  </div>

  <div id="remoteBrowseOverlay" class="modal-overlay hidden">
    <div class="modal modal-browse" role="dialog" aria-modal="true" aria-labelledby="remoteBrowseTitle">
      <div class="modal-title" id="remoteBrowseTitle">浏览远端目录</div>
      <div class="remote-browse-path-row">
        <input id="remoteBrowsePath" type="text" spellcheck="false" />
        <button class="secondary small" id="remoteBrowseGoBtn" type="button">前往</button>
      </div>
      <div class="remote-browse-status" id="remoteBrowseStatus"></div>
      <p class="hint remote-browse-hint" id="remoteBrowseHint">双击文件夹进入；双击文件可选中</p>
      <div class="remote-browse-list" id="remoteBrowseList"></div>
      <div class="remote-browse-new-dir hidden" id="remoteBrowseNewDirRow">
        <input id="remoteBrowseNewDirName" type="text" placeholder="新目录名称" spellcheck="false" />
        <button class="secondary small" id="remoteBrowseCreateDirBtn" type="button">创建</button>
        <button class="ghost small" id="remoteBrowseCancelNewDirBtn" type="button">取消</button>
      </div>
      <div class="modal-actions remote-browse-actions">
        <button class="secondary" id="remoteBrowseUpBtn" type="button">上级目录</button>
        <button class="secondary" id="remoteBrowseNewDirBtn" type="button">新建目录</button>
        <button class="secondary" id="remoteBrowseSelectDirBtn" type="button">选择此目录</button>
        <button class="primary" id="remoteBrowseCloseBtn" type="button">关闭</button>
      </div>
    </div>
  </div>
`;

const profileSelect = document.querySelector<HTMLSelectElement>("#profileSelect")!;
const profileNameInput = document.querySelector<HTMLInputElement>("#profileName")!;
const connectionToggle = document.querySelector<HTMLButtonElement>("#connectionToggle")!;
const connectionBody = document.querySelector<HTMLDivElement>("#connectionBody")!;
const connectionSummary = document.querySelector<HTMLSpanElement>("#connectionSummary")!;
const connectionStatus = document.querySelector<HTMLDivElement>("#connectionStatus")!;
const authHint = document.querySelector<HTMLParagraphElement>("#authHint")!;
const connectionCard = document.querySelector<HTMLDivElement>("#connectionCard")!;
const fileCard = document.querySelector<HTMLDivElement>("#fileCard")!;
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
const transferSettingsToggle = document.querySelector<HTMLButtonElement>("#transferSettingsToggle")!;
const transferSettingsBody = document.querySelector<HTMLDivElement>("#transferSettingsBody")!;
const transferSettingsSummary = document.querySelector<HTMLSpanElement>("#transferSettingsSummary")!;
const transferBtn = document.querySelector<HTMLButtonElement>("#transferBtn")!;
const cancelBtn = document.querySelector<HTMLButtonElement>("#cancelBtn")!;
const clearCheckpointBtn = document.querySelector<HTMLButtonElement>("#clearCheckpointBtn")!;
const progressWrap = document.querySelector<HTMLDivElement>("#progressWrap")!;
const progressBadge = document.querySelector<HTMLSpanElement>("#progressBadge")!;
const progressDetail = document.querySelector<HTMLDivElement>("#progressDetail")!;
const progressSpeed = document.querySelector<HTMLDivElement>("#progressSpeed")!;
const progressStats = document.querySelector<HTMLDivElement>("#progressStats")!;
const verifySummary = document.querySelector<HTMLDivElement>("#verifySummary")!;
const transferActivity = document.querySelector<HTMLDivElement>("#transferActivity")!;
const transferScene = document.querySelector<HTMLDivElement>("#transferScene")!;
const progressToolbar = document.querySelector<HTMLDivElement>("#progressToolbar")!;
const progressBar = document.querySelector<HTMLProgressElement>("#progressBar")!;
const statusText = document.querySelector<HTMLDivElement>("#statusText")!;
const historyPeek = document.querySelector<HTMLDivElement>("#historyPeek")!;
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
type RunnerMood = "fast" | "steady" | "struggle" | "waiting" | "connecting" | "verifying";
let runnerMood: RunnerMood = "steady";
let runnerMoodCandidate: RunnerMood = "steady";
let runnerMoodCandidateSince = 0;
let transferDirection: TransferDirection = "upload";
let retryCount = 0;
let connectionExpanded = false;
let transferSettingsExpanded = false;
let historyEntries: UploadHistoryEntry[] = [];
let savedProfiles: ConnectionProfile[] = [];
let activeUploadCheckpoint: UploadCheckpoint | null = null;
let activeDownloadCheckpoint: DownloadCheckpoint | null = null;
let lastProbe: RemoteUploadProbe | RemoteDownloadProbe | null = null;
let lastProgressBarBytes = 0;
let lastProgressEventAt = 0;
let speedSamples: { t: number; bytes: number }[] = [];
let speedDisplayTimer: number | null = null;
let progressWatchdogTimer: number | null = null;
const UPLOAD_CHUNK_BYTES = 1024 * 1024;
const SPEED_SAMPLE_WINDOW_MS = 3000;
const SPEED_REFRESH_MS = 1000;
const SPEED_STALE_MS = 8_000;
/** 动画角色：≥512 KB/s 视为「快」 */
const RUNNER_MOOD_FAST_BPS = 512 * 1024;
/** 动画角色：<256 KB/s 视为「慢」 */
const RUNNER_MOOD_SLOW_BPS = 256 * 1024;
/** 切到「快/正常」前需稳定一段时间，避免网速抖动频繁变脸 */
const RUNNER_MOOD_HOLD_MS = 3000;
const UI_STALL_WARN_MS = 10_000;
const CONFIRM_ANIM_MS = 200;
/** 路径预检仅在失焦或程序化改路径时触发，避免每次按键都连 SSH */
const PROBE_DEBOUNCE_MS = 400;
const WINDOW_MIN_HEIGHT = 460;
const WINDOW_MAX_HEIGHT = 820;
const WINDOW_HEIGHT_PAD = 12;

let fitWindowTimer: number | null = null;

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
  "#connectionToggle",
  "#transferSettingsToggle",
] as const;

let probeDebounceTimer: number | null = null;
let backgroundProbeGeneration = 0;

function scheduleFitWindow() {
  // 传输中布局已固定，避免 ResizeObserver / 进度刷新触发反复 setSize 卡顿
  if (isTransferring) return;
  if (fitWindowTimer !== null) {
    window.clearTimeout(fitWindowTimer);
  }
  fitWindowTimer = window.setTimeout(() => {
    fitWindowTimer = null;
    void fitWindowToContent();
  }, 80);
}

async function fitWindowToContent() {
  try {
    const win = getCurrentWindow();
    const current = await win.innerSize();
    const mainScroll = document.querySelector(".main-scroll");
    const app = document.querySelector("#app");
    if (!mainScroll || !app) return;

    const appStyles = getComputedStyle(app);
    const verticalPad =
      parseFloat(appStyles.paddingTop) + parseFloat(appStyles.paddingBottom);
    // 用内容区真实高度，避免窗口拉高后 scrollHeight 被视口撑满无法缩回
    const contentHeight = Math.ceil(mainScroll.scrollHeight + verticalPad);

    const target = Math.min(
      Math.max(contentHeight + WINDOW_HEIGHT_PAD, WINDOW_MIN_HEIGHT),
      WINDOW_MAX_HEIGHT,
    );
    if (Math.abs(current.height - target) > 6) {
      await win.setSize(new LogicalSize(current.width, target));
    }
  } catch {
    // 浏览器预览模式下忽略
  }
}

function setupWindowAutoFit() {
  const root = document.querySelector(".main-scroll");
  if (!root) return;
  const observer = new ResizeObserver(() => scheduleFitWindow());
  observer.observe(root);
}

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
  for (const selector of TRANSFER_LOCK_SELECTORS) {
    const el = document.querySelector<HTMLInputElement | HTMLSelectElement | HTMLButtonElement>(
      selector,
    );
    if (el) el.disabled = locked;
  }
  localPathInput.readOnly = locked;
  remotePathInput.readOnly = locked;
  connectionCard.classList.toggle("transfer-locked", locked);
  fileCard.classList.toggle("transfer-active", locked);
  transferBtn.classList.toggle("hidden", locked);
}

function setTransferring(running: boolean) {
  isTransferring = running;
  transferBtn.disabled = running;
  testConnBtn.disabled = running;
  setTransferFormLocked(running);
  cancelBtn.classList.toggle("hidden", !running);
  cancelBtn.textContent = "取消";
  cancelBtn.classList.remove("is-cancelling");
  progressToolbar.classList.toggle("is-active", running);
  progressWrap.classList.toggle("is-transferring", running);
  transferActivity.classList.toggle("hidden", !running);
  transferActivity.setAttribute("aria-hidden", running ? "false" : "true");
  if (running) {
    isConnecting = true;
    clearProbeDebounce();
    backgroundProbeGeneration++;
    lastProbe = null;
    hideVerifySummary();
    historyPeek.classList.add("hidden");
    statusText.classList.add("hidden");
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
    updateHistoryPeek();
  }
  updateTransferStatePanel();
  scheduleFitWindow();
  updateTransferActivityMood();
}

function updateTransferDirectionUI() {
  const download = isDownloadMode();
  localPathLabel.textContent = download ? "保存到" : "本地文件";
  remotePathLabel.textContent = download ? "远端文件" : "远端路径";
  localPathInput.placeholder = download
    ? "本地目录（以 / 结尾）或完整文件路径"
    : "选择或拖拽文件到此处";
  remotePathInput.placeholder = download
    ? "/home/user/file.jar"
    : "/home/user/dir/ 或 /home/user/file.jar";
  pathHint.textContent = download
    ? "保存路径填目录时会自动拼接远端文件名"
    : "填目录时以 / 结尾；若远端已是同名目录，也会自动拼接文件名";
  dropHint.textContent = download
    ? "下载模式下请用「选择」按钮指定保存目录"
    : "支持将文件拖拽到此处";
  dropZone.classList.toggle("download-mode", download);
  forceOverwriteLabel.textContent = download
    ? "强制从头覆盖（忽略断点与本地已有文件）"
    : "强制从头覆盖（忽略断点与远端已有数据）";
  transferBtn.textContent = download ? "开始下载" : "开始上传";
  browseRemoteBtn.textContent = download ? "浏览文件" : "浏览目录";
  updateRemoteBrowseModeUi();
  updateResolvedPathHint();
  updateTransferStatePanel();
}

function setUploading(running: boolean) {
  setTransferring(running);
}

function isReliableMode(): boolean {
  const selected = document.querySelector<HTMLInputElement>(
    'input[name="transferMode"]:checked',
  );
  return selected?.value !== "fast";
}

function updateModeHint() {
  if (isReliableMode()) {
    modeHint.textContent = "弱网下自动逐块校验，速度略慢但更可靠";
  } else {
    modeHint.textContent = "仅传完后做完整性校验，速度更快";
  }
  updateTransferSettingsSummary();
}

function updateTransferSettingsSummary() {
  let text = isReliableMode() ? "弱网可靠" : "快速传输";
  if (forceOverwriteInput.checked) {
    text += " · 强制覆盖";
  }
  transferSettingsSummary.textContent = text;
}

function setTransferSettingsExpanded(expanded: boolean) {
  transferSettingsExpanded = expanded;
  transferSettingsBody.classList.toggle("collapsed", !expanded);
  transferSettingsToggle.classList.toggle("expanded", expanded);
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
    return { ready: false, summary: "未配置连接" };
  }

  const endpoint = user && host ? `${user}@${host}:${port}` : host || user;
  const issues: string[] = [];

  if (!host) issues.push("缺主机");
  if (!user) issues.push("缺用户名");

  const needsPassword = authTypeSelect.value === "password";
  const missingPassword = needsPassword && !passwordInput.value;
  const missingKey = !needsPassword && !keyPathInput.value.trim();

  if (missingPassword) issues.push("缺密码");
  if (missingKey) issues.push("缺私钥");
  if (!profileSelected && (missingPassword || missingKey)) {
    issues.unshift("未选连接");
  }

  if (issues.length === 0) {
    const summary = profileLabel
      ? `${profileLabel.split(" (")[0]} · ${endpoint}`
      : endpoint;
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
  if (state.summary === "未配置连接") {
    summaryText = "未配置连接 · 点击展开";
  } else if (state.detail) {
    summaryText = `${state.summary} · ${state.detail}`;
  } else {
    summaryText = state.summary;
  }
  connectionSummary.textContent = summaryText;
  connectionSummary.title = summaryText;
  connectionSummary.classList.toggle("ok", state.ready);
  connectionSummary.classList.toggle("warn", !state.ready);
  connectionToggle.classList.toggle("connection-incomplete", !state.ready);
  updateAuthHintVisibility();
}

function updateAuthHintVisibility() {
  const profileSelected = profileSelect.value !== "";
  const passwordReady =
    authTypeSelect.value !== "password" || Boolean(passwordInput.value);
  authHint.classList.toggle("hidden", profileSelected && passwordReady);
}

function validateConnectionBeforeTransfer(): void {
  const state = getConnectionReadiness();
  if (state.ready) return;

  setConnectionExpanded(true);

  const host = hostInput.value.trim();
  const user = usernameInput.value.trim();
  const profileSelected = profileSelect.value !== "";

  if (!host || !user) {
    throw new Error(
      "连接信息不完整。请展开连接设置，从「快速连接」选择已保存连接，或填写主机与用户名",
    );
  }

  if (authTypeSelect.value === "password" && !passwordInput.value) {
    throw new Error(
      profileSelected
        ? "所选连接未保存密码。请展开连接设置填写密码后点「保存」，或直接输入密码再传输"
        : "未选择已保存连接，且密码为空。请在「快速连接」中选择连接，或展开连接设置填写密码",
    );
  }

  if (authTypeSelect.value === "key" && !keyPathInput.value.trim()) {
    throw new Error("请展开连接设置，选择私钥文件");
  }

  throw new Error(
    state.detail
      ? `连接未就绪（${state.detail}）。请展开连接设置检查`
      : "连接未就绪，请展开连接设置检查",
  );
}

function setConnectionExpanded(expanded: boolean) {
  connectionExpanded = expanded;
  connectionBody.classList.toggle("collapsed", !expanded);
  connectionToggle.classList.toggle("expanded", expanded);
}

function analyzeCheckpointState(
  cp: UploadCheckpoint | DownloadCheckpoint,
  download: boolean,
): TransferStateView {
  const transferred = getTransferredBytes(cp);
  const total = cp.file_size;
  const pct = total > 0 ? Math.round((transferred / total) * 100) : 0;
  const action = download ? "开始下载" : "开始上传";
  const verifyReadFailed = cp.failure_reason === "verify_read_failed";
  const needsRedo =
    cp.failure_reason === "hash_mismatch" ||
    (cp.status === "failed" && transferred >= total && !verifyReadFailed);

  if (verifyReadFailed && transferred >= total) {
    return {
      kind: "info",
      title: "传输已完成，待最终校验",
      detail: `各数据块均已通过校验（${formatBytes(total)}）`,
      hint: `直接点「${action}」重试校验即可，无需从头重传`,
      showClear: true,
    };
  }
  if (needsRedo) {
    return {
      kind: "error",
      title: download ? "校验未通过，需重新下载" : "校验未通过，需重新上传",
      detail: `已传输 ${formatBytes(transferred)}，但完整性校验失败`,
      hint: "请勾选「强制覆盖」或先清除断点后再试",
      showClear: true,
    };
  }
  if (transferred > 0 && transferred < total) {
    return {
      kind: "info",
      title: `可续传（${pct}%）`,
      detail: `${download ? "已下" : "已传"} ${formatBytes(transferred)} / ${formatBytes(total)}`,
      hint: `点击「${action}」将从断点继续`,
      showClear: true,
    };
  }
  if (transferred === 0 && cp.status === "failed") {
    return {
      kind: "warn",
      title: download ? "上次下载未完成" : "上次上传未完成",
      detail: "断点记录无有效进度",
      hint: `点击「${action}」将重新传输；若远端/本地已有文件会自动处理`,
      showClear: true,
    };
  }
  return {
    kind: "warn",
    title: download ? "检测到未完成下载" : "检测到未完成上传",
    detail: transferred > 0 ? `${formatBytes(transferred)} / ${formatBytes(total)}` : undefined,
    hint: `点击「${action}」继续`,
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
      title: download ? "将强制覆盖本地文件" : "将强制覆盖远端文件",
      detail: probe.message,
      hint: "开始传输前会删除已有文件并从头重来",
      showClear: false,
    };
  }

  const action = probe.action;
  if (action === "verify_retry") {
    return {
      kind: "info",
      title: "传输已完成，待最终校验",
      detail: probe.message,
      hint: download ? "直接开始下载即可重试校验" : "直接开始上传即可重试校验",
      showClear: false,
    };
  }
  if (action === "full_reupload" || action === "full_redownload") {
    return {
      kind: "error",
      title: action === "full_redownload" ? "需从头重下" : "需从头重传",
      detail: probe.message,
      hint: "继续将删除已有文件并重新传输",
      showClear: false,
    };
  }
  if (action === "overwrite") {
    return {
      kind: "warn",
      title: download ? "将覆盖本地文件" : "将覆盖远端文件",
      detail: probe.message,
      showClear: false,
    };
  }
  if (action === "resume") {
    return {
      kind: "info",
      title: "可续传",
      detail: probe.message,
      showClear: false,
    };
  }
  return {
    kind: "neutral",
    title: download ? "可以开始下载" : "可以开始上传",
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
  scheduleFitWindow();
}

function updateTransferStatePanel() {
  if (isTransferring) {
    hideTransferStatePanel();
    return;
  }

  const cp = getActiveCheckpoint();
  if (cp && cp.status !== "completed") {
    renderTransferStatePanel(analyzeCheckpointState(cp, isDownloadMode()));
    scheduleFitWindow();
    return;
  }

  if (lastProbe) {
    renderTransferStatePanel(analyzeProbeState(lastProbe, isDownloadMode()));
    scheduleFitWindow();
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

function showConfirm(title: string, body: string): Promise<boolean> {
  if (confirmResolver) {
    const stale = confirmResolver;
    confirmResolver = null;
    stale(false);
  }

  confirmTitle.textContent = title;
  confirmBody.textContent = body;
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
  remoteBrowseTitle.textContent = download ? "浏览远端文件" : "浏览远端目录";
  remoteBrowseSelectDirBtn.classList.toggle("hidden", download);
  remoteBrowseSelectDirBtn.textContent = "选择此目录";
  remoteBrowseHint.textContent = download
    ? "双击文件夹进入；单击文件选中"
    : "双击文件夹进入；双击文件选中";
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
    empty.textContent = "此目录为空";
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
    icon.textContent = entry.is_dir ? "📁" : "📄";

    const name = document.createElement("span");
    name.className = "remote-browse-name";
    name.textContent = entry.name;

    const meta = document.createElement("span");
    meta.className = "remote-browse-meta";
    meta.textContent = entry.is_dir ? "目录" : formatBytes(entry.size);

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
    setRemoteBrowseStatus("请输入目录名称", "error");
    return;
  }

  const parentPath = remoteBrowseCurrent?.path ?? remoteBrowsePath.value.trim();
  if (!parentPath) {
    setRemoteBrowseStatus("请先进入父目录", "error");
    return;
  }

  remoteBrowseCreateDirBtn.disabled = true;
  setRemoteBrowseStatus("正在创建…");

  try {
    validateConnectionBeforeTransfer();
    const listing = await invoke<RemoteDirListing>("create_remote_dir", {
      request: {
        ...connectionPayload(),
        parent_path: parentPath,
        dir_name: dirName,
      },
    });
    hideRemoteBrowseNewDirForm();
    renderRemoteBrowseListing(listing);
    setRemoteBrowseStatus(`已创建「${dirName}」`);
  } catch (error) {
    setRemoteBrowseStatus(String(error), "error");
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
  setRemoteBrowseStatus("正在加载…");

  try {
    validateConnectionBeforeTransfer();
    const listing = await invoke<RemoteDirListing>("list_remote_dir", {
      request: {
        ...connectionPayload(),
        path: path?.trim() || remoteBrowsePath.value.trim() || null,
      },
    });
    renderRemoteBrowseListing(listing);
    setRemoteBrowseStatus(`${listing.entries.length} 项`);
  } catch (error) {
    setRemoteBrowseStatus(String(error), "error");
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
    validateConnectionBeforeTransfer();
  } catch (error) {
    setStatus(String(error), "error");
    setConnectionExpanded(true);
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
  if (!localPath) throw new Error("请填写本地保存路径");
  if (!remotePath) throw new Error("请填写远端文件路径");

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
      "强制覆盖下载",
      "将删除本地已有文件并从头重下，是否继续？",
    );
  }

  if (probe.action === "full_redownload") {
    return showConfirm(
      "需要从头重下",
      `${probe.message}\n\n将删除本地已有文件并重新下载，是否继续？`,
    );
  }

  if (probe.action === "overwrite") {
    return showConfirm(
      "覆盖本地文件",
      `${probe.message}\n\n本地已有同名文件，继续将覆盖重下，是否继续？`,
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
    const message = String(error);
    if (!isTransferring && message.includes("已有下载任务在进行中")) {
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
  const probe = await probeRemoteDownload();
  const confirmed = await confirmDownloadAction(probe);
  if (!confirmed) return;

  const forceOverwrite =
    probe.action === "full_redownload" || forceOverwriteInput.checked;

  setTransferring(true);
  retryCount = 0;
  resetSpeedTracker();
  isConnecting = true;
  setProgressState("uploading");
  applyActiveTransferBadge("download");
  showProgressSpeed("正在建立 SSH 连接…");

  await invokeStartDownload(forceOverwrite);
}

async function probeRemoteUpload(): Promise<RemoteUploadProbe> {
  validateConnectionBeforeTransfer();
  const localPath = localPathInput.value.trim();
  const remotePath = remotePathInput.value.trim();
  if (!localPath) throw new Error("请选择本地文件");
  if (!remotePath) throw new Error("请填写远端路径");

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
      "强制覆盖上传",
      "将删除远端已有文件并从头重传，是否继续？",
    );
  }

  if (probe.action === "full_reupload") {
    return showConfirm(
      "需要从头重传",
      `${probe.message}\n\n将删除远端已有文件并重新上传，是否继续？`,
    );
  }

  if (probe.action === "overwrite") {
    return showConfirm(
      "覆盖远端文件",
      `${probe.message}\n\n远端已有同名文件，继续将覆盖重传，是否继续？`,
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
    const message = String(error);
    if (!isTransferring && message.includes("已有上传任务在进行中")) {
      await invoke("reset_upload_state");
      await invoke("start_upload", {
        request: uploadPayload(forceOverwrite),
      });
      return;
    }
    throw error;
  }
}

async function startUploadFlow() {
  const probe = await probeRemoteUpload();
  const confirmed = await confirmUploadAction(probe);
  if (!confirmed) return;

  const forceOverwrite =
    probe.action === "full_reupload" || forceOverwriteInput.checked;

  setUploading(true);
  retryCount = 0;
  resetSpeedTracker();
  isConnecting = true;
  setProgressState("uploading");
  applyActiveTransferBadge("upload");
  showProgressSpeed("正在建立 SSH 连接…");

  await invokeStartUpload(forceOverwrite);
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
  return new Date(ts * 1000).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
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

function clarifyTransferError(message: string): string {
  if (message.includes("远端路径错误") || message.includes("远端路径「")) {
    return message;
  }
  if (message.includes("请填写密码")) {
    return "未填写密码。请展开连接设置，从「快速连接」选择已保存连接，或手动填写密码";
  }
  if (
    /认证失败|服务器拒绝了提供的凭据/i.test(message) &&
    authTypeSelect.value === "password" &&
    !passwordInput.value
  ) {
    return "未填写密码。请展开连接设置，从「快速连接」选择已保存连接，或手动填写密码";
  }
  if (/认证失败|服务器拒绝了提供的凭据/i.test(message)) {
    return `SSH 认证失败，密码或私钥可能不正确。\n${message}`;
  }
  const remote = remotePathInput.value.trim();
  if (/no such file|sftp\(2\)/i.test(message) && remote) {
    return `远端路径错误：${remote}\n该路径或上级目录在服务器上不存在（本地文件不受影响）`;
  }
  if (/SFTP 错误/i.test(message) && remote && !message.includes(remote)) {
    return `远端路径错误：${remote}\n${message}`;
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
  const badgeActive = direction === "upload" ? "上传中" : "下载中";
  if (isConnecting) {
    setProgressState("uploading");
    setBadge("连接中", "retrying");
    return;
  }
  if (progressWrap.classList.contains("stalled")) {
    setProgressState("retrying");
    setBadge("等待响应", "stalled");
    return;
  }
  if (isReconnecting) {
    setProgressState("retrying");
    setBadge("重试中", "retrying");
    return;
  }
  setProgressState("uploading");
  setBadge(badgeActive, "uploading");
}

/** 数据流动画流速随网速与传输阶段变化 */
function updateTransferActivityMood() {
  if (!isTransferring) {
    runnerMood = "steady";
    runnerMoodCandidate = "steady";
    applyRunnerMood("steady");
    return;
  }

  const target = resolveRunnerMood();
  const now = Date.now();

  if (target === "waiting" || target === "struggle" || target === "connecting" || target === "verifying") {
    runnerMood = target;
    runnerMoodCandidate = target;
    runnerMoodCandidateSince = now;
    applyRunnerMood(target);
    return;
  }

  if (target !== runnerMoodCandidate) {
    runnerMoodCandidate = target;
    runnerMoodCandidateSince = now;
  }

  if (target !== runnerMood && now - runnerMoodCandidateSince >= RUNNER_MOOD_HOLD_MS) {
    runnerMood = target;
    applyRunnerMood(target);
  }
}

function resolveRunnerMood(): RunnerMood {
  if (isConnecting) {
    return "connecting";
  }
  if (isFullVerifying) {
    return "verifying";
  }
  if (isChunkVerifying) {
    return "steady";
  }
  if (
    isReconnecting ||
    progressWrap.classList.contains("stalled") ||
    (lastProgressEventAt > 0 && Date.now() - lastProgressEventAt >= SPEED_STALE_MS)
  ) {
    return "waiting";
  }

  const speed = estimateTransferSpeedBps();
  if (speed === null) return "steady";
  if (speed >= RUNNER_MOOD_FAST_BPS) return "fast";
  if (speed < RUNNER_MOOD_SLOW_BPS) return "struggle";
  return "steady";
}

function applyRunnerMood(mood: RunnerMood) {
  transferScene.classList.remove("mood-fast", "mood-steady", "mood-struggle", "mood-waiting");
  // connecting/verifying 无独立粒子样式，复用 steady 表现
  const visual =
    mood === "connecting" || mood === "verifying" ? "steady" : mood;
  transferScene.classList.add(`mood-${visual}`);
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
  scheduleFitWindow();
}

function applyThemePreference(mode: ThemePreference) {
  const root = document.documentElement;
  if (mode === "system") {
    root.removeAttribute("data-theme");
  } else {
    root.dataset.theme = mode;
  }
  localStorage.setItem(THEME_STORAGE_KEY, mode);
}

function initThemePreference() {
  const saved = (localStorage.getItem(THEME_STORAGE_KEY) as ThemePreference | null) ?? "system";
  const radio = document.querySelector<HTMLInputElement>(
    `input[name="themeMode"][value="${saved}"]`,
  );
  if (radio) radio.checked = true;
  applyThemePreference(saved);
}

function formatProgressDetail(transferred: number, total: number, retries: number): string {
  const pct = formatProgressPercent(transferred, total);
  const size = `${formatBytes(transferred)} / ${formatBytes(total)}`;
  const base = pct ? `${pct} · ${size}` : size;
  return retries > 0 ? `${base} · 重试 ${retries} 次` : base;
}

function syncProgressStatsRow() {
  const showDetail = !progressDetail.classList.contains("hidden") && progressDetail.textContent;
  const showSpeed = !progressSpeed.classList.contains("hidden") && progressSpeed.textContent;
  progressStats.classList.toggle("hidden", !showDetail && !showSpeed);
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
  return remaining > 0 ? `约 ${remaining}s 后重试` : "";
}

/** Stall/retry 副文本：动态计时，避免用户误以为界面卡死 */
function showWaitingSpeedText(elapsedMs: number) {
  const secs = Math.floor(elapsedMs / 1000);
  const delayHint = formatRetryDelayHint();
  const details: string[] = [];
  if (isReconnecting && retryCount > 0) details.push(`第 ${retryCount} 次重试`);
  if (delayHint) details.push(delayHint);
  details.push(`已等待 ${secs}s`);
  const prefix = isReconnecting ? "重连中..." : "等待响应...";
  showProgressSpeed(`${prefix} (${details.join(" · ")})`, true);
}

/** 刚进入重连时的副文本 */
function showReconnectingSpeedText() {
  const delayHint = formatRetryDelayHint();
  const details: string[] = [];
  if (retryCount > 0) details.push(`第 ${retryCount} 次重试`);
  if (delayHint) details.push(delayHint);
  const prefix = "重连中...";
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
  if (transferred > 0 && total > 0 && isTransferring) {
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
  scheduleFitWindow();
}

function hideTransferStatus() {
  if (isTransferring) {
    statusText.classList.add("hidden");
    statusText.textContent = "";
    scheduleFitWindow();
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
  if (isTransferring) return;
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

function restoreCredentialsFromProfile(
  host: string,
  port: number,
  username: string,
  authKind: "password" | "private_key",
) {
  const profile = savedProfiles.find(
    (p) => p.host === host && p.port === port && p.username === username,
  );
  if (!profile) return;

  profileSelect.value = profile.id;
  profileNameInput.value = profile.name;

  if (authKind === "password" && profile.password && !passwordInput.value) {
    passwordInput.value = profile.password;
  }
  if (authKind === "private_key" && profile.key_passphrase && !keyPassphraseInput.value) {
    keyPassphraseInput.value = profile.key_passphrase;
  }

  setConnectionStatus(`已匹配已保存连接「${profile.name}」`, "ok");
  updateConnectionSummary();
}

function formatEtaSeconds(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "";
  if (seconds < 60) return `${Math.ceil(seconds)} 秒`;
  if (seconds < 3600) return `${Math.ceil(seconds / 60)} 分钟`;
  return `${(seconds / 3600).toFixed(1)} 小时`;
}

function formatProgressPercent(transferred: number, total: number): string {
  if (total <= 0) return "";
  const pct = Math.min(100, Math.round((transferred / total) * 100));
  return `${pct}%`;
}

function updateProgress(transferred: number, total: number) {
  const percent = total > 0 ? Math.round((transferred / total) * 100) : 0;
  progressBar.value = percent;
  progressBar.max = 100;
  progressBar.dataset.totalBytes = String(total);
  progressWrap.classList.toggle("has-progress", transferred > 0 && percent < 100);
  updateProgressDetail(transferred, total);
}

function updateProgressBarIfNeeded(uploaded: number, total: number, force = false) {
  const crossedChunk =
    Math.floor(uploaded / UPLOAD_CHUNK_BYTES) >
    Math.floor(lastProgressBarBytes / UPLOAD_CHUNK_BYTES);
  if (force || crossedChunk || uploaded >= total) {
    updateProgress(uploaded, total);
    lastProgressBarBytes = uploaded;
  }
}

function resetSpeedTracker() {
  lastProgressEventAt = 0;
  lastProgressBarBytes = 0;
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
    updateTransferActivityMood();
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
    updateTransferActivityMood();
    return;
  }

  let speedText = `${formatBytes(speed)}/s`;
  const transferred = speedSamples[speedSamples.length - 1]?.bytes ?? 0;
  const total = Number(progressBar.dataset.totalBytes ?? 0);
  if (total > transferred && speed > 0) {
    const eta = formatEtaSeconds((total - transferred) / speed);
    if (eta) speedText += ` · 约 ${eta}`;
  }
  showProgressSpeed(speedText);
  updateTransferActivityMood();
}

function refreshStallUi() {
  if (!isTransferring || lastProgressEventAt === 0) return;

  if (isConnecting) {
    applyActiveTransferBadge(isDownloadMode() ? "download" : "upload");
    showProgressSpeed("正在建立 SSH 连接…");
    updateTransferActivityMood();
    return;
  }

  if (isChunkVerifying) {
    return;
  }

  const elapsed = Date.now() - lastProgressEventAt;
  if (elapsed < SPEED_STALE_MS && !isReconnecting) return;

  if (elapsed >= SPEED_STALE_MS) {
    showWaitingSpeedText(elapsed);
    updateTransferActivityMood();
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
  const hasPath = path.length > 0;
  dropZone.classList.toggle("has-file", hasPath);
  fileChip.classList.toggle("hidden", !hasPath);
  dropZone.classList.toggle("hidden", hasPath);

  if (hasPath) {
    const name = path.split("/").pop() || path.split("\\").pop() || path;
    fileChipName.textContent = name;
    fileChipPath.textContent = path;
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
  const resolved = previewResolvedPath();
  const remote = remotePathInput.value.trim();
  if (resolved && resolved !== remote && resolved !== localPathInput.value.trim()) {
    const label = isDownloadMode() ? "实际保存至" : "实际上传至";
    resolvedPathHint.textContent = `${label}：${resolved}`;
    resolvedPathHint.classList.remove("hidden");
  } else {
    resolvedPathHint.classList.add("hidden");
  }
}

function updateHistoryPeek() {
  const host = hostInput.value.trim();
  const recent = historyEntries.find((e) => e.host === host);
  if (!recent || isTransferring) {
    historyPeek.classList.add("hidden");
    return;
  }
  const dir = recent.direction === "download" ? "下载" : "上传";
  const status =
    recent.status === "completed" ? "完成" : recent.status === "cancelled" ? "取消" : "失败";
  const name = recent.local_path.split("/").pop() ?? recent.local_path;
  historyPeek.textContent = `本机最近：${dir} ${name} · ${status} · ${formatTime(recent.finished_at)}`;
  historyPeek.classList.remove("hidden");
  scheduleFitWindow();
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

  passwordInput.value = profile.password ?? "";
  keyPassphraseInput.value = profile.key_passphrase ?? "";

  profileNameInput.value = profile.name;
  updateAuthFields();
  updateConnectionSummary();
  setConnectionExpanded(false);

  if (profile.auth_kind === "password") {
    setConnectionStatus(
      profile.password
        ? `已加载「${profile.name}」，密码已自动填入`
        : `已加载「${profile.name}」，请填写密码后点保存`,
      profile.password ? "ok" : "normal",
    );
  } else {
    setConnectionStatus(`已加载「${profile.name}」`, "ok");
  }
}

async function loadHistory() {
  historyEntries = await invoke<UploadHistoryEntry[]>("list_upload_history");
  updateHistoryPeek();
}

async function loadProfiles() {
  savedProfiles = await invoke<ConnectionProfile[]>("list_saved_profiles");
  profileSelect.replaceChildren();
  const defaultOption = document.createElement("option");
  defaultOption.value = "";
  defaultOption.textContent = "选择已保存连接";
  profileSelect.appendChild(defaultOption);

  for (const profile of savedProfiles) {
    const option = document.createElement("option");
    option.value = profile.id;
    option.textContent = `${profile.name} (${profile.username}@${profile.host})`;
    profileSelect.appendChild(option);
  }
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
  setConnectionExpanded(false);
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
  restoreCredentialsFromProfile(
    checkpoint.host,
    checkpoint.port,
    checkpoint.username,
    checkpoint.auth_kind,
  );
  updateDropZoneState();
  updateResolvedPathHint();
  updateConnectionSummary();
  setConnectionExpanded(false);

  updateProgress(checkpoint.uploaded_bytes, checkpoint.file_size);
  setProgressState("idle");

  const transferred = getTransferredBytes(checkpoint);
  const verifyReadFailed = checkpoint.failure_reason === "verify_read_failed";
  const needsFullReupload =
    checkpoint.failure_reason === "hash_mismatch" ||
    (checkpoint.status === "failed" &&
      transferred >= checkpoint.file_size &&
      !verifyReadFailed);

  if (!options?.keepStatus) {
    if (verifyReadFailed) {
      setBadge("待校验", "retrying");
      setStatus(
        "各数据块已校验通过，仅最终校验时网络中断。直接点「开始上传」即可重试，无需从头重传。",
        "retry",
      );
    } else if (needsFullReupload) {
      setBadge("需重传", "error");
      progressBar.value = 0;
      setStatus(
        "上次校验未通过，远端文件可能已损坏。请勾选强制覆盖或清除断点后重新上传。",
        "error",
      );
    } else if (transferred > 0) {
      setBadge("可续传", "uploading");
      setStatus(
        `已恢复断点：${formatBytes(transferred)} / ${formatBytes(checkpoint.file_size)}`,
      );
    } else {
      setBadge("待重试", "retrying");
      setStatus(
        "上次传输未完成且无有效进度，点击「开始上传」将重新传输",
        "retry",
      );
    }
  }
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
  restoreCredentialsFromProfile(
    checkpoint.host,
    checkpoint.port,
    checkpoint.username,
    checkpoint.auth_kind,
  );
  updateDropZoneState();
  updateResolvedPathHint();
  updateConnectionSummary();
  setConnectionExpanded(false);

  updateProgress(checkpoint.downloaded_bytes, checkpoint.file_size);
  setProgressState("idle");

  const transferred = getTransferredBytes(checkpoint);
  const verifyReadFailed = checkpoint.failure_reason === "verify_read_failed";
  const needsFullRedownload =
    checkpoint.failure_reason === "hash_mismatch" ||
    (checkpoint.status === "failed" &&
      transferred >= checkpoint.file_size &&
      !verifyReadFailed);

  if (verifyReadFailed) {
    setBadge("待校验", "retrying");
    setStatus(
      "各数据块已校验通过，仅最终校验时网络中断。直接点「开始下载」即可重试，无需从头重下。",
      "retry",
    );
  } else if (needsFullRedownload) {
    setBadge("需重下", "error");
    progressBar.value = 0;
    setStatus(
      "上次校验未通过，本地文件可能已损坏。请勾选强制覆盖或清除断点后重新下载。",
      "error",
    );
  } else if (transferred > 0) {
    setBadge("可续传", "uploading");
    setStatus(
      `已恢复断点：${formatBytes(transferred)} / ${formatBytes(checkpoint.file_size)}`,
    );
  } else {
    setBadge("待重试", "retrying");
    setStatus(
      "上次传输未完成且无有效进度，点击「开始下载」将重新传输",
      "retry",
    );
  }
  updateTransferStatePanel();
}

async function restoreActiveCheckpoint() {
  await restoreUploadCheckpoint();
  await restoreDownloadCheckpoint();
}

function setupDragDrop() {
  void getCurrentWebview().onDragDropEvent((event) => {
    if (isDownloadMode()) return;
    if (event.payload.type === "over") {
      dropZone.classList.add("drag-over");
    } else if (event.payload.type === "leave") {
      dropZone.classList.remove("drag-over");
    } else if (event.payload.type === "drop") {
      dropZone.classList.remove("drag-over");
      const path = event.payload.paths[0];
      if (path) {
        localPathInput.value = path;
        updateDropZoneState();
        updateResolvedPathHint();
        scheduleBackgroundProbe();
      }
    }
  });
}

authTypeSelect.addEventListener("change", updateAuthFields);

profileSelect.addEventListener("change", async () => {
  const id = profileSelect.value;
  if (!id) {
    updateConnectionSummary();
    return;
  }
  const profiles = await invoke<ConnectionProfile[]>("list_saved_profiles");
  const profile = profiles.find((p) => p.id === id);
  if (profile) await applyProfile(profile);
});

saveProfileBtn.addEventListener("click", async () => {
  const name = profileNameInput.value.trim() || defaultProfileName();
  if (!name) {
    setConnectionStatus("请先填写连接名称，或选择 SSH Config Host", "error");
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
      },
    });
    await loadProfiles();
    profileSelect.value = saved.id;
    profileNameInput.value = saved.name;
    const stored =
      authTypeSelect.value === "password" && passwordInput.value
        ? "，密码已保存"
        : keyPassphraseInput.value
          ? "，私钥口令已保存"
          : "";
    setConnectionStatus(`已保存为「${saved.name}」${stored}`, "ok");
  } catch (error) {
    setConnectionStatus(String(error), "error");
  }
});

deleteProfileBtn.addEventListener("click", async () => {
  const id = profileSelect.value;
  if (!id) {
    setConnectionStatus("请先选择要删除的连接", "error");
    return;
  }
  const confirmed = await showConfirm(
    "删除连接",
    "确定删除这个已保存的连接？",
  );
  if (!confirmed) return;

  try {
    await invoke("delete_connection_profile", { id });
    profileSelect.value = "";
    profileNameInput.value = "";
    await loadProfiles();
    setConnectionStatus("连接配置已删除", "ok");
  } catch (error) {
    setConnectionStatus(String(error), "error");
  }
});

hostSelect.addEventListener("change", () => {
  void applyHostSelection(hostSelect.value);
});

[hostInput, usernameInput, portInput].forEach((input) => {
  input.addEventListener("input", () => {
    updateConnectionSummary();
    if (!profileNameInput.value.trim()) {
      profileNameInput.placeholder = defaultProfileName() || "保存名称，如 myserver 或 user@host";
    }
  });
});

[passwordInput, keyPathInput, keyPassphraseInput].forEach((input) => {
  input.addEventListener("input", () => {
    updateConnectionSummary();
    updateAuthHintVisibility();
  });
});

connectionToggle.addEventListener("click", () => {
  setConnectionExpanded(!connectionExpanded);
});

transferSettingsToggle.addEventListener("click", () => {
  setTransferSettingsExpanded(!transferSettingsExpanded);
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
    validateConnectionBeforeTransfer();
    testConnBtn.disabled = true;
    setBadge("测试中");
    setStatus("正在测试连接...");
    const message = await invoke<string>("test_connection", {
      request: connectionPayload(),
    });
    setBadge("连接正常", "uploading");
    setConnectionStatus(message, "ok");
    setStatus(message);
  } catch (error) {
    setBadge("连接失败", "error");
    setConnectionStatus(String(error), "error");
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

document.querySelector("#pickFileBtn")!.addEventListener("click", async () => {
  const selected = await open({
    multiple: false,
    directory: isDownloadMode(),
  });
  if (typeof selected === "string") {
    localPathInput.value = isDownloadMode() && !selected.endsWith("/")
      ? `${selected}/`
      : selected;
    updateDropZoneState();
    updateResolvedPathHint();
    clearProbeResult();
    scheduleBackgroundProbe();
  }
});

changeFileBtn.addEventListener("click", async () => {
  localPathInput.value = "";
  updateDropZoneState();
  updateResolvedPathHint();
  clearProbeResult();
  const selected = await open({
    multiple: false,
    directory: isDownloadMode(),
  });
  if (typeof selected === "string") {
    localPathInput.value = isDownloadMode() && !selected.endsWith("/")
      ? `${selected}/`
      : selected;
    updateDropZoneState();
    updateResolvedPathHint();
    scheduleBackgroundProbe();
  }
});

forceOverwriteInput.addEventListener("change", () => {
  updateTransferSettingsSummary();
  if (lastProbe) updateTransferStatePanel();
});

document.querySelectorAll<HTMLInputElement>('input[name="themeMode"]').forEach((radio) => {
  radio.addEventListener("change", () => {
    if (!radio.checked) return;
    applyThemePreference(radio.value as ThemePreference);
  });
});

[hostInput].forEach((input) => {
  input.addEventListener("input", updateHistoryPeek);
});

localPathInput.addEventListener("input", () => {
  updateDropZoneState();
  updateResolvedPathHint();
  invalidateProbeOnEdit();
});

localPathInput.addEventListener("blur", () => {
  if (!isTransferring) void runBackgroundProbe();
});

remotePathInput.addEventListener("input", () => {
  updateResolvedPathHint();
  invalidateProbeOnEdit();
});

remotePathInput.addEventListener("blur", () => {
  if (!isTransferring) void runBackgroundProbe();
});

transferBtn.addEventListener("click", async () => {
  try {
    if (isDownloadMode()) {
      await startDownloadFlow();
    } else {
      await startUploadFlow();
    }
  } catch (error) {
    setTransferring(false);
    setBadge("失败", "error");
    setStatus(String(error), "error");
  }
});


cancelBtn.addEventListener("click", () => {
  if (cancelInFlight || !isTransferring) return;

  cancelInFlight = true;
  cancelBtn.disabled = true;
  cancelBtn.textContent = "取消中…";
  cancelBtn.classList.add("is-cancelling");
  setBadge("取消中", "retrying");
  setStatus("正在取消...");

  const cmd = isDownloadMode() ? "cancel_download" : "cancel_upload";
  void invoke(cmd).catch((error) => {
    cancelInFlight = false;
    if (isTransferring) {
      cancelBtn.disabled = false;
      cancelBtn.textContent = "取消";
      cancelBtn.classList.remove("is-cancelling");
    }
    setStatus(String(error), "error");
  });
});

clearCheckpointBtn.addEventListener("click", async () => {
  const download = isDownloadMode();
  const confirmed = await showConfirm(
    "清除断点",
    download
      ? "确定清除下载断点记录？\n\n清除后下次下载将从头开始，本地已下载的部分不会自动删除。"
      : "确定清除断点记录？\n\n清除后下次上传将从头开始，已传到远端的数据不会自动删除。",
  );
  if (!confirmed) return;

  if (download) {
    await invoke("clear_saved_download_checkpoint");
    activeDownloadCheckpoint = null;
  } else {
    await invoke("clear_saved_checkpoint");
    activeUploadCheckpoint = null;
  }
  clearCheckpointBtn.classList.add("hidden");
  progressBar.value = 0;
  progressWrap.classList.remove("has-progress");
  updateTransferStatePanel();
  setBadge("等待开始");
  setStatus("断点记录已清除");
  progressDetail.classList.add("hidden");
  syncProgressStatsRow();
});


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
    updateTransferActivityMood();
    return;
  }

  if (status === "completed") {
    updateProgressBarIfNeeded(transferred, total, true);
    setTransferring(false);
    setProgressState("idle");
    progressWrap.classList.remove("has-progress");
    resetSpeedTracker();
    setBadge("已完成", "uploading");
    setStatus(message);
    showVerifySummary(
      verify_summary ??
        (isReliableMode()
          ? "校验通过 · 逐块校验 + SHA-256 一致"
          : "校验通过 · SHA-256 一致"),
    );
    if (uploading) activeUploadCheckpoint = null;
    else activeDownloadCheckpoint = null;
    updateTransferStatePanel();
    void loadHistory();
    return;
  }

  if (status === "cancelled") {
    setTransferring(false);
    setProgressState("idle");
    resetSpeedTracker();
    hideVerifySummary();
    setBadge("已取消", "retrying");
    setStatus(message);
    void restoreActiveCheckpoint();
    void loadHistory();
    return;
  }

  if (status === "cancelling") {
    cancelBtn.disabled = true;
    cancelBtn.textContent = "取消中…";
    cancelBtn.classList.add("is-cancelling");
    setProgressState("retrying");
    setBadge("取消中", "retrying");
    showTransferStatus(message, "retry");
    return;
  }

  if (status === "failed") {
    setTransferring(false);
    setProgressState("idle");
    resetSpeedTracker();
    hideVerifySummary();
    setBadge("失败", "error");
    setStatus(message, "error");
    void refreshCheckpointFromDisk();
    void loadHistory();
    return;
  }

  if (status === "verifying") {
    isChunkVerifying = false;
    isFullVerifying = true;
    markProgressActivity();
    setProgressState("verifying");
    setBadge("校验中", "verifying");
    updateProgressBarIfNeeded(transferred, total, true);
    updateProgressDetail(transferred, total, retry_count);
    showTransferStatus("正在校验文件完整性，请稍候…");
    showProgressSpeed("校验中");
    updateTransferActivityMood();
    return;
  }

  if (status === "chunk_verifying") {
    // 块读回校验对用户透明，外观与正常传输一致，仅刷新心跳避免误报 stall
    isChunkVerifying = true;
    markProgressActivity();
    applyActiveTransferBadge(direction);
    updateProgressBarIfNeeded(transferred, total);
    updateProgressDetail(transferred, total, retry_count);
    hideTransferStatus();
    updateTransferActivityMood();
    return;
  }

  isChunkVerifying = false;
  isFullVerifying = false;

  if (status === "retrying") {
    isReconnecting = true;
    setProgressState("retrying");
    setBadge("重试中", "retrying");
    showReconnectingSpeedText();
    updateProgressDetail(transferred, total, retry_count);
    showTransferStatus(message, "retry");
    updateTransferActivityMood();
    return;
  }

  applyActiveTransferBadge(direction);
  recordSpeedSample(transferred);
  updateProgressBarIfNeeded(transferred, total);
  updateProgressDetail(transferred, total, retry_count);
  refreshSpeedDisplay();
  hideTransferStatus();
  updateTransferActivityMood();
}

void listen("transfer-close-requested", async () => {
  const ok = await showConfirm(
    "传输进行中",
    "关闭窗口将取消当前传输，已传输的部分会保留为断点。\n\n确定关闭吗？",
  );
  if (!ok) return;

  await invoke("prepare_app_close");
  await getCurrentWindow().close();
});

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
      setProgressState("uploading");
      setBadge(state.download_running ? "下载中" : "上传中", "uploading");
      setStatus("检测到进行中的传输任务…");
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
  setProgressState("retrying");
  setBadge("重试中", "retrying");
  showReconnectingSpeedText();
  showTransferStatus(event.payload.message, "retry");
  updateTransferActivityMood();
});

void listen<DownloadRetryEvent>("download-retry", (event) => {
  retryCount = event.payload.retry_count;
  retryDelayMs = event.payload.delay_ms;
  retryScheduledAt = Date.now();
  isReconnecting = true;
  isConnecting = false;
  setProgressState("retrying");
  setBadge("重试中", "retrying");
  showReconnectingSpeedText();
  showTransferStatus(event.payload.message, "retry");
  updateTransferActivityMood();
});

setupDragDrop();
setupWindowAutoFit();
initThemePreference();
updateConnectionSummary();
updateModeHint();
updateTransferDirectionUI();
setConnectionExpanded(!hostInput.value.trim());
void loadProfiles();
void loadHosts();
void restoreActiveCheckpoint().then(() => scheduleFitWindow());
void loadHistory();
void syncTransferStateOnStartup().then(() => scheduleFitWindow());
updateAuthFields();
updateDropZoneState();
updateResolvedPathHint();
updateAuthHintVisibility();
updateRemoteBrowseModeUi();
scheduleFitWindow();
