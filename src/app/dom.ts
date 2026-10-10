// 一次性渲染应用骨架，并导出各模块用到的 DOM 节点和可视化组件实例。
import { ActivityLogView } from "../ui/activityLog";
import { BlockMap } from "../ui/blockMap";
import { Popover } from "../ui/popover";
import { appTemplate } from "../ui/template";
import { UPLOAD_CHUNK_BYTES } from "./constants";
import { readThemePreference, resolveTheme } from "./settings";

// 在渲染前同步应用主题，避免启动时先闪一下浅色。
document.documentElement.dataset.theme = resolveTheme(readThemePreference());


export const APP_NAME = "ResiliSSH";


export const app = document.querySelector<HTMLDivElement>("#app")!;


app.innerHTML = appTemplate(APP_NAME);


export const profileSelect = document.querySelector<HTMLSelectElement>("#profileSelect")!;

export const profileNameInput = document.querySelector<HTMLInputElement>("#profileName")!;

export const connectionSummary = document.querySelector<HTMLSpanElement>("#connectionSummary")!;

export const connectionStatus = document.querySelector<HTMLDivElement>("#connectionStatus")!;

export const authHint = document.querySelector<HTMLParagraphElement>("#authHint")!;

export const rememberSecretsInput = document.querySelector<HTMLInputElement>("#rememberSecrets")!;

export const rememberRow = document.querySelector<HTMLLabelElement>("#rememberRow")!;

export const saveProfileBtn = document.querySelector<HTMLButtonElement>("#saveProfileBtn")!;

export const deleteProfileBtn = document.querySelector<HTMLButtonElement>("#deleteProfileBtn")!;

export const hostSelect = document.querySelector<HTMLSelectElement>("#hostSelect")!;

export const hostInput = document.querySelector<HTMLInputElement>("#host")!;

export const portInput = document.querySelector<HTMLInputElement>("#port")!;

export const usernameInput = document.querySelector<HTMLInputElement>("#username")!;

export const authTypeSelect = document.querySelector<HTMLSelectElement>("#authType")!;
export const jumpProfileSelect = document.querySelector<HTMLSelectElement>("#jumpProfile")!;

export const keyFields = document.querySelector<HTMLDivElement>("#keyFields")!;

export const passwordFields = document.querySelector<HTMLDivElement>("#passwordFields")!;

export const keyPathInput = document.querySelector<HTMLInputElement>("#keyPath")!;

export const keyPassphraseInput = document.querySelector<HTMLInputElement>("#keyPassphrase")!;

export const passwordInput = document.querySelector<HTMLInputElement>("#password")!;

export const testConnBtn = document.querySelector<HTMLButtonElement>("#testConnBtn")!;

export const transferStatePanel = document.querySelector<HTMLDivElement>("#transferStatePanel")!;

export const statePanelTitle = document.querySelector<HTMLDivElement>("#statePanelTitle")!;

export const statePanelDetail = document.querySelector<HTMLDivElement>("#statePanelDetail")!;

export const statePanelHint = document.querySelector<HTMLDivElement>("#statePanelHint")!;

export const localPathLabel = document.querySelector<HTMLLabelElement>("#localPathLabel")!;

export const remotePathLabel = document.querySelector<HTMLLabelElement>("#remotePathLabel")!;

export const fileChip = document.querySelector<HTMLDivElement>("#fileChip")!;

export const fileChipName = document.querySelector<HTMLDivElement>("#fileChipName")!;

export const fileChipPath = document.querySelector<HTMLDivElement>("#fileChipPath")!;

export const changeFileBtn = document.querySelector<HTMLButtonElement>("#changeFileBtn")!;

export const dropHint = document.querySelector<HTMLParagraphElement>("#dropHint")!;

export const pathHint = document.querySelector<HTMLParagraphElement>("#pathHint")!;

export const forceOverwriteLabel = document.querySelector<HTMLSpanElement>("#forceOverwriteLabel")!;

export const localPathInput = document.querySelector<HTMLInputElement>("#localPath")!;

export const remotePathInput = document.querySelector<HTMLInputElement>("#remotePath")!;

export const browseRemoteBtn = document.querySelector<HTMLButtonElement>("#browseRemoteBtn")!;

export const resolvedPathHint = document.querySelector<HTMLParagraphElement>("#resolvedPathHint")!;

export const dropZone = document.querySelector<HTMLDivElement>("#dropZone")!;

export const forceOverwriteInput = document.querySelector<HTMLInputElement>("#forceOverwrite")!;

export const modeHint = document.querySelector<HTMLParagraphElement>("#modeHint")!;

export const transferBtn = document.querySelector<HTMLButtonElement>("#transferBtn")!;

export const cancelBtn = document.querySelector<HTMLButtonElement>("#cancelBtn")!;

export const clearCheckpointBtn = document.querySelector<HTMLButtonElement>("#clearCheckpointBtn")!;

export const progressWrap = document.querySelector<HTMLDivElement>("#progressWrap")!;

export const progressBadge = document.querySelector<HTMLSpanElement>("#progressBadge")!;

export const progressDetail = document.querySelector<HTMLDivElement>("#progressDetail")!;

export const progressSpeed = document.querySelector<HTMLDivElement>("#progressSpeed")!;

export const verifySummary = document.querySelector<HTMLDivElement>("#verifySummary")!;

export const statusText = document.querySelector<HTMLDivElement>("#statusText")!;

export const progressPercent = document.querySelector<HTMLDivElement>("#progressPercent")!;

export const runSummary = document.querySelector<HTMLDivElement>("#runSummary")!;

export const mapCaption = document.querySelector<HTMLSpanElement>("#mapCaption")!;

export const composeView = document.querySelector<HTMLDivElement>("#composeView")!;

export const runView = document.querySelector<HTMLDivElement>("#runView")!;

export const resultView = document.querySelector<HTMLDivElement>("#resultView")!;

export const resultCard = document.querySelector<HTMLDivElement>("#resultCard")!;

export const resultIcon = document.querySelector<HTMLDivElement>("#resultIcon")!;

export const resultTitle = document.querySelector<HTMLDivElement>("#resultTitle")!;

export const resultSubtitle = document.querySelector<HTMLDivElement>("#resultSubtitle")!;

export const resultStats = document.querySelector<HTMLDListElement>("#resultStats")!;

export const resultBackBtn = document.querySelector<HTMLButtonElement>("#resultBackBtn")!;

export const editServerBtn = document.querySelector<HTMLButtonElement>("#editServerBtn")!;

export const newServerBtn = document.querySelector<HTMLButtonElement>("#newServerBtn")!;

export const serverList = document.querySelector<HTMLDivElement>("#serverList")!;

export const pendingSection = document.querySelector<HTMLElement>("#pendingSection")!;

export const pendingList = document.querySelector<HTMLDivElement>("#pendingList")!;

export const historyList = document.querySelector<HTMLDivElement>("#historyList")!;

export const navTransferLive = document.querySelector<HTMLSpanElement>("#navTransferLive")!;

export const navServerCount = document.querySelector<HTMLSpanElement>("#navServerCount")!;

export const navHistoryBadge = document.querySelector<HTMLSpanElement>("#navHistoryBadge")!;

export const miniTransfer = document.querySelector<HTMLButtonElement>("#miniTransfer")!;

export const miniTransferLabel = document.querySelector<HTMLSpanElement>("#miniTransferLabel")!;

export const miniTransferPct = document.querySelector<HTMLSpanElement>("#miniTransferPct")!;

export const miniTransferFill = document.querySelector<HTMLDivElement>("#miniTransferFill")!;

export const miniTransferSpeed = document.querySelector<HTMLDivElement>("#miniTransferSpeed")!;

export const appVersion = document.querySelector<HTMLSpanElement>("#appVersion")!;

export const viewLogBtn = document.querySelector<HTMLButtonElement>("#viewLogBtn")!;

export const bookmarkBtn = document.querySelector<HTMLButtonElement>("#bookmarkBtn")!;

export const pathMenuBtn = document.querySelector<HTMLButtonElement>("#pathMenuBtn")!;

export const pathMenu = new Popover(
  document.querySelector<HTMLDivElement>("#pathMenu")!,
  document.querySelector<HTMLInputElement>("#remotePath")!.closest<HTMLElement>(".path-field")!,
);

export const queueChip = document.querySelector<HTMLSpanElement>("#queueChip")!;

export const resultFiles = document.querySelector<HTMLUListElement>("#resultFiles")!;

export const postCommandInput = document.querySelector<HTMLInputElement>("#postCommand")!;

export const saveCommandBtn = document.querySelector<HTMLButtonElement>("#saveCommandBtn")!;

export const commandMenuBtn = document.querySelector<HTMLButtonElement>("#commandMenuBtn")!;

export const commandMenu = new Popover(
  document.querySelector<HTMLDivElement>("#commandMenu")!,
  document.querySelector<HTMLDivElement>("#commandField")!,
);

export const commandConsole = document.querySelector<HTMLDivElement>("#commandConsole")!;

export const commandConsoleCmd = document.querySelector<HTMLSpanElement>("#commandConsoleCmd")!;

export const commandOutput = document.querySelector<HTMLPreElement>("#commandOutput")!;

export const resultCommand = document.querySelector<HTMLDivElement>("#resultCommand")!;

export const resultCommandLine = document.querySelector<HTMLDivElement>("#resultCommandLine")!;

export const resultCommandOutput = document.querySelector<HTMLPreElement>("#resultCommandOutput")!;

export const legendResumed = document.querySelector<HTMLSpanElement>("#legendResumed")!;

export const legendRetry = document.querySelector<HTMLSpanElement>("#legendRetry")!;

export const mapLegend = document.querySelector<HTMLDivElement>("#mapLegend")!;

export const legendDoneLabel = document.querySelector<HTMLSpanElement>("#legendDoneLabel")!;

export const activityLog = new ActivityLogView(
  document.querySelector<HTMLDivElement>("#logList")!,
  document.querySelector<HTMLButtonElement>("#logJumpBtn")!,
  document.querySelector<HTMLSpanElement>("#navLogBadge")!,
);

export const confirmOverlay = document.querySelector<HTMLDivElement>("#confirmOverlay")!;

export const confirmTitle = document.querySelector<HTMLDivElement>("#confirmTitle")!;

export const confirmBody = document.querySelector<HTMLDivElement>("#confirmBody")!;

export const confirmCancel = document.querySelector<HTMLButtonElement>("#confirmCancel")!;

export const confirmOk = document.querySelector<HTMLButtonElement>("#confirmOk")!;

export const remoteBrowseOverlay = document.querySelector<HTMLDivElement>("#remoteBrowseOverlay")!;

export const remoteBrowseTitle = document.querySelector<HTMLDivElement>("#remoteBrowseTitle")!;

export const remoteBrowsePath = document.querySelector<HTMLInputElement>("#remoteBrowsePath")!;

export const remoteBrowseGoBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseGoBtn")!;

export const remoteBrowseStatus = document.querySelector<HTMLDivElement>("#remoteBrowseStatus")!;

export const remoteBrowseHint = document.querySelector<HTMLParagraphElement>("#remoteBrowseHint")!;

export const remoteBrowseList = document.querySelector<HTMLDivElement>("#remoteBrowseList")!;

export const remoteBrowseUpBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseUpBtn")!;

export const remoteBrowseSelectDirBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseSelectDirBtn")!;

export const remoteBrowseNewDirBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseNewDirBtn")!;

export const remoteBrowseNewDirRow = document.querySelector<HTMLDivElement>("#remoteBrowseNewDirRow")!;

export const remoteBrowseNewDirName = document.querySelector<HTMLInputElement>("#remoteBrowseNewDirName")!;

export const remoteBrowseCreateDirBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseCreateDirBtn")!;

export const remoteBrowseCancelNewDirBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseCancelNewDirBtn")!;

export const remoteBrowseCloseBtn = document.querySelector<HTMLButtonElement>("#remoteBrowseCloseBtn")!;


export const blockMap = new BlockMap(
  document.querySelector<HTMLCanvasElement>("#blockMap")!,
  UPLOAD_CHUNK_BYTES,
);


export const pickFolderBtn = document.querySelector<HTMLButtonElement>("#pickFolderBtn")!;

export const languageHint = document.querySelector<HTMLParagraphElement>("#languageHint")!;


export const copyLogBtn = document.querySelector<HTMLButtonElement>("#copyLogBtn")!;
