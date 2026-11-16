// 单个文件的上传/下载流程：启动、进度事件处理、完成与失败的收尾。
import { t } from "../i18n";
import { icons } from "../ui/icons";
import { downloadDir } from "@tauri-apps/api/path";
import { invoke } from "@tauri-apps/api/core";
import { clarifyTransferError } from "./backendText";
import { commandContextFor, describeCommandFailure, runPostCommand, showCommandResult, updateSaveCommandButton } from "./command";
import { connectionPayload, prepareConnection } from "./connection";
import { TRANSFER_LOCK_SELECTORS, UI_STALL_WARN_MS } from "./constants";
import { blockMap, browseRemoteBtn, cancelBtn, composeView, dropHint, dropZoneIcon, hostInput, pickFileBtn, forceOverwriteInput, languageHint, forceOverwriteLabel, localPathInput, localPathLabel, modeHint, pathHint, pickFolderBtn, postCommandInput, progressDetail, progressWrap, remotePathInput, remotePathLabel, resultBackBtn, resultCard, resultCommand, resultIcon, resultStats, resultSubtitle, resultTitle, resultView, runView, statusText, testConnBtn, transferBtn, localField, remoteField, verifySummary, viewLogBtn } from "./dom";
import { asDir, fileNameOf, formatBytes, formatDuration, prefersReducedMotion } from "./format";
import { loadHistory, refreshCheckpointFromDisk, restoreActiveCheckpoint } from "./history";
import { isQueueSelection, updateBookmarkButton, updateResolvedPathHint } from "./paths";
import { clearLastProbe, clearProbeDebounce, confirmDownloadAction, confirmUploadAction, probeRemoteDownload, probeRemoteUpload, showDownloadProbeResult, showProbeResult, updateTransferStatePanel } from "./probe";
import { applyActiveTransferBadge, beginRunVisuals, hideProgressSpeed, hideTransferStatus, hideVerifySummary, markProgressActivity, recordSpeedSample, refreshLiveVisuals, refreshSpeedDisplay, resetSpeedTracker, setBadge, setProgressState, showProgressSpeed, showReconnectingSpeedText, showTransferStatus, showVerifySummary, showWaitingSpeedText, startProgressWatchdog, stopProgressWatchdog, updateProgress, updateProgressDetail } from "./progress";
import { clearDockProgress } from "./dock";
import { recordReceipt, syncReceiptVisibility } from "./receipt";
import { updateRemoteBrowseModeUi } from "./remoteBrowse";
import { clearSavedBatch } from "./savedBatch";
import { readParallelConnections } from "./settings";
import { isBusy, state } from "./state";
import { ByteRange, DownloadCheckpoint, ReceiptFile, ResultKind, RunStats, TransferDirection, TransferView, UploadCheckpoint, UploadStart } from "./types";

export function isDownloadMode(): boolean {
  return state.transferDirection === "download";
}


export function getActiveCheckpoint(): UploadCheckpoint | DownloadCheckpoint | null {
  return isDownloadMode() ? state.activeDownloadCheckpoint : state.activeUploadCheckpoint;
}


export function getTransferredBytes(cp: UploadCheckpoint | DownloadCheckpoint): number {
  if ("uploaded_bytes" in cp) return cp.uploaded_bytes;
  return cp.downloaded_bytes;
}


export function setTransferFormLocked(locked: boolean) {
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


/**
 * 按 isBusy() 统一锁定或解锁「开始」「测试连接」和整张表单。所有会改变忙碌状态的地方
 * （传输起止、队列起止、传完后命令起止、开始流程起止）改完状态后都调用它，
 * 而不是各自去开关控件：批量的文件间隙里 isTransferring 会短暂为 false，
 * 各管各的就会在那一瞬间把表单放开，下一个文件读到的可能已是被改过的服务器。
 */
export function syncBusyLock() {
  const busy = isBusy();
  transferBtn.disabled = busy;
  testConnBtn.disabled = busy;
  setTransferFormLocked(busy);
  // setTransferFormLocked 会连方向单选一起放开，方向还要看当前视图。
  syncDirectionLock();
  if (busy) return;
  // 解锁是「一律启用」，而这两个星标还取决于是否选了已保存的服务器、是否填了内容。
  updateBookmarkButton();
  updateSaveCommandButton();
  // 「完成后再切换语言」的提示只在忙碌时成立。
  languageHint.textContent = "";
}


/** 取消按钮恢复为可用的「取消传输」：一次取消（单个文件或整个批量）了结后调用。 */
export function resetCancelButton() {
  state.cancelInFlight = false;
  cancelBtn.disabled = false;
  cancelBtn.textContent = t("取消传输");
}


/**
 * 开始流程的阶段提示（连接服务器、检查远端文件）。点「开始」到进入进行中视图之间
 * 要做主机指纹确认和一次完整的预检连接，弱网下可达数秒；只禁用按钮看起来就像卡死了。
 */
export function showStartPhase(message: string) {
  state.startPhaseText = message;
  showTransferStatus(message);
}


/** 只清掉自己写的阶段提示：期间如果已被错误信息等替换，就保留那条信息。 */
export function clearStartPhase() {
  if (state.startPhaseText && statusText.textContent === state.startPhaseText) {
    statusText.textContent = "";
  }
  state.startPhaseText = "";
}


export function setTransferring(running: boolean) {
  state.isTransferring = running;
  // 批量中每个文件结束都会短暂置为 false，此时不清，免得 Dock 进度在文件之间闪烁；整批结束时由队列清除。
  if (!running && !state.activeQueue) clearDockProgress();
  syncBusyLock();
  if (running) {
    // 上一次如果以「停止命令」收尾，按钮文案还停在那里。
    cancelBtn.textContent = t("取消传输");
    state.isConnecting = true;
    state.runDirection = state.transferDirection;
    clearProbeDebounce();
    clearLastProbe();
    hideVerifySummary();
    statusText.textContent = "";
    beginRunVisuals();
    showTransferView("run");
    startProgressWatchdog();
  } else {
    resetCancelButton();
    state.isReconnecting = false;
    state.isConnecting = false;
    state.isChunkVerifying = false;
    state.isFullVerifying = false;
    state.retryDelayMs = 0;
    state.retryScheduledAt = 0;
    stopProgressWatchdog();
    progressDetail.classList.add("hidden");
    hideProgressSpeed();
    statusText.classList.remove("hidden");
  }
  updateTransferStatePanel();
  refreshLiveVisuals();
}


/** 传输页三种视图互斥：编辑 → 进行中 → 结果；页脚按钮随视图切换，主操作位置保持不变。 */
export function showTransferView(view: TransferView) {
  state.currentView = view;
  // 进入进行中视图后由视图自己说明状态，页脚的阶段提示不再需要。
  if (view === "run") clearStartPhase();
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
export function syncDirectionLock() {
  document
    .querySelectorAll<HTMLInputElement>('input[name="transferDirection"]')
    .forEach((el) => {
      el.disabled = state.currentView !== "compose";
    });
}


export function updateTransferButtonLabel() {
  const verb = isDownloadMode() ? t("下载") : t("上传");
  // 成功后唯一的下一步是「再传一个」，把它升级为主按钮；其他情况它只是次要的「返回修改」。
  // 「传完但命令失败」也算传输成功：下一步同样是「再传一个」，重试传输没有意义。
  const successResult = state.currentView === "result" && resultCard.dataset.transferOk === "1";
  resultBackBtn.classList.toggle("btn-primary", successResult);
  resultBackBtn.classList.toggle("btn-large", successResult);
  resultBackBtn.classList.toggle("btn-secondary", !successResult);
  if (successResult) {
    transferBtn.classList.add("hidden");
    resultBackBtn.textContent = t("再{verb}一个", { verb });
    return;
  }
  resultBackBtn.textContent = t("返回修改");
  if (state.currentView === "result") {
    const queued = isDownloadMode() ? state.selectedDownloads.length : state.selectedUploads.length;
    const retryLabel = isQueueSelection() ? t("重试 {length} 个文件", { length: queued }) : t("重试");
    transferBtn.innerHTML = resultCard.classList.contains("paused") ? t("继续传输") : retryLabel;
  } else {
    // 用整句翻译而不是拼接动词：后端提示里会引用按钮原文（如 click "Start upload"），两边必须一致。
    transferBtn.innerHTML = `${isDownloadMode() ? t("开始下载") : t("开始上传")} <kbd>⌘↩</kbd>`;
  }
}


export function updateTransferDirectionUI() {
  const download = isDownloadMode();
  // 字段顺序跟着传输方向走（从哪来 → 到哪去）：下载时远端文件在前、保存位置在后，填写时不用倒着想。
  if (download) localField.before(remoteField);
  else remoteField.before(localField);
  localPathLabel.textContent = download ? t("保存到") : t("本地文件");
  remotePathLabel.textContent = download ? t("远端文件") : t("远端路径");
  localPathInput.placeholder = download
    ? t("本地目录（以 / 结尾）或完整文件路径")
    : t("或粘贴本地路径");
  remotePathInput.placeholder = download
    ? "/home/user/file.jar"
    : t("/home/user/dir/ 或 /home/user/file.jar");
  pathHint.textContent = download
    ? t("保存路径填目录时自动拼接远端文件名")
    : t("以 / 结尾表示目录，会自动拼接文件名");
  dropHint.textContent = download ? t("选择下载后保存到的文件夹") : t("把文件或文件夹拖到这里");
  // 下载时本地一侧选的是保存目录：图标、按钮文案跟着换，多余的「选择文件夹」收起（同一个按钮已经在选文件夹）。
  dropZoneIcon.innerHTML = download ? icons.download : icons.upload;
  pickFileBtn.textContent = download ? t("选择文件夹…") : t("选择文件…");
  pickFolderBtn.classList.toggle("hidden", download);
  // 「传完后执行」是把文件送到服务器之后的动作，下载用不上。
  document.querySelector("#commandField")!.classList.toggle("hidden", download);
  forceOverwriteLabel.textContent = t("强制从头覆盖");
  forceOverwriteInput.parentElement!.title = download
    ? t("忽略断点与本地已有文件，从头重新下载")
    : t("忽略断点与远端已有数据，从头重新上传");
  updateTransferButtonLabel();
  browseRemoteBtn.textContent = t("浏览…");
  updateRemoteBrowseModeUi();
  updateResolvedPathHint();
  updateTransferStatePanel();
  void prefillDownloadDir();
}


/**
 * 下载时保存位置默认填系统的「下载」文件夹：绝大多数下载就是要放到那里，
 * 省掉每次都得先选一遍目录这一步；想存到别处照常点「更换」。已经填了路径时不动它。
 */
async function prefillDownloadDir() {
  const applies = () => isDownloadMode() && !localPathInput.value.trim() && !isBusy();
  if (!applies()) return;
  try {
    const dir = await downloadDir();
    // 取目录是异步的：期间用户可能已经切回上传或自己填了路径。
    if (!dir || !applies()) return;
    localPathInput.value = asDir(dir);
    localPathInput.dispatchEvent(new Event("input"));
  } catch (error) {
    // 取不到只是少一个默认值，用户仍可手动选择。
    console.warn("cannot resolve the Downloads folder", error);
  }
}


export function isReliableMode(): boolean {
  const selected = document.querySelector<HTMLInputElement>(
    'input[name="transferMode"]:checked',
  );
  return selected?.value !== "fast";
}


export function updateModeHint() {
  modeHint.textContent = isReliableMode()
    ? t("每块写完立即校验，最稳")
    : t("只在传完后整体校验，速度更快");
}


/** 上传与下载的启动参数结构相同，方向由调用的后端命令决定。 */
export function transferPayload(forceOverwrite: boolean) {
  return {
    ...connectionPayload(),
    local_path: localPathInput.value.trim(),
    remote_path: remotePathInput.value.trim(),
    strict_chunk_verify: isReliableMode(),
    force_overwrite: forceOverwrite,
    // 弱网下多开几条连接各传文件的不同部分；小文件后端无论如何只用一条。批量中的每个文件用同一个值。
    connections: readParallelConnections(),
    // 批量中的文件由队列结束时整体记一条历史，避免一次批量挤掉全部历史。
    in_batch: Boolean(state.activeQueue),
  };
}


export async function startDownloadFlow() {
  await prepareConnection();
  showStartPhase(t("正在检查远端文件…"));
  const outcome = await startOneDownload();
  if (outcome !== "declined") clearSavedBatch();
  if (outcome === "identical") await finishIdenticalTransfer("download");
}


/** 预检、确认、启动一个下载；与 startOneUpload 对称，批量下载逐个调用它。 */
export async function startOneDownload(): Promise<UploadStart> {
  const probe = await probeRemoteDownload();
  clearStartPhase();
  // 批量中用户在预检期间点了取消：这个文件不再启动，交回队列收尾。
  if (state.queueCancelRequested) return "declined";
  // 本地磁盘不够时直接拦下，理由与上传相同。
  if (probe.action === "insufficient_space") {
    showDownloadProbeResult(probe);
    throw new Error(probe.message);
  }
  if (probe.action === "identical" && !forceOverwriteInput.checked) {
    showDownloadProbeResult(probe);
    state.runRemotePath = probe.resolved_remote_path;
    state.runLocalPath = probe.resolved_local_path;
    // 传完后命令只用于上传，下载不执行（该字段在下载模式下是隐藏的）。
    state.runPostCommandTemplate = "";
    return "identical";
  }
  const confirmed = await confirmDownloadAction(probe);
  if (!confirmed) return "declined";

  const forceOverwrite =
    probe.action === "full_redownload" || forceOverwriteInput.checked;
  state.runRemotePath = probe.resolved_remote_path;
  state.runLocalPath = probe.resolved_local_path;
  state.runPostCommandTemplate = "";

  setTransferring(true);
  state.retryCount = 0;
  resetSpeedTracker();
  state.isConnecting = true;
  setProgressState("uploading");
  applyActiveTransferBadge("download");
  showProgressSpeed(t("正在建立 SSH 连接…"));

  await invoke("start_download", { request: transferPayload(forceOverwrite) });
  return "started";
}


export async function startUploadFlow() {
  await prepareConnection();
  showStartPhase(t("正在检查远端文件…"));
  const outcome = await startOneUpload();
  // 用户转去传别的文件了：之前没传完的批量不再提示继续（其中传到一半的文件仍有自己的断点）。
  // 在确认框里放弃则不算，那次传输并没有开始。
  if (outcome !== "declined") clearSavedBatch();
  if (outcome === "identical") await finishIdenticalTransfer("upload");
}


/**
 * 目标端已有完全相同的文件：不传输，直接按「已送达」收尾。
 * 传完后命令照常执行——文件确实已就位且校验一致，部署流程不应因为「没变化」而中断。
 */
export async function finishIdenticalTransfer(direction: TransferDirection) {
  const download = direction === "download";
  state.runDirection = direction;
  beginRunVisuals();
  showTransferView("run");
  const probe = state.lastProbe;
  const size = probe ? (download ? probe.remote_size : probe.local_size) : 0;
  blockMap.update(size, size);
  blockMap.setPhase("completed");
  showVerifySummary(t("远端 SHA-256 与本地一致"));
  await finishSuccessfulRun(
    download ? t("本地已是最新") : t("远端已是最新"),
    download ? t("本地已有完全相同的文件，无需下载") : t("远端已有完全相同的文件，无需上传"),
    { elapsedMs: 0, movedBytes: 0, retries: 0 },
    [state.runRemotePath],
    [{ local: state.runLocalPath, remote: state.runRemotePath, skipped: true }],
  );
}


export async function startOneUpload(): Promise<UploadStart> {
  const probe = await probeRemoteUpload();
  clearStartPhase();
  // 批量中用户在预检期间点了取消：这个文件不再启动，交回队列收尾。
  if (state.queueCancelRequested) return "declined";
  // 空间不足时直接拦下：传到 90% 才因磁盘满失败，比现在就报错糟糕得多。
  if (probe.action === "insufficient_space") {
    showProbeResult(probe);
    throw new Error(probe.message);
  }
  // 勾了「强制从头覆盖」说明用户明确要重传，此时不走跳过逻辑。
  if (probe.action === "identical" && !forceOverwriteInput.checked) {
    showProbeResult(probe);
    state.runRemotePath = probe.resolved_remote_path;
    state.runLocalPath = localPathInput.value.trim();
    state.runPostCommandTemplate = postCommandInput.value.trim();
    return "identical";
  }
  const confirmed = await confirmUploadAction(probe);
  if (!confirmed) return "declined";

  const forceOverwrite =
    probe.action === "full_reupload" || forceOverwriteInput.checked;
  state.runRemotePath = probe.resolved_remote_path;
  state.runLocalPath = localPathInput.value.trim();
  state.runPostCommandTemplate = postCommandInput.value.trim();

  setTransferring(true);
  state.retryCount = 0;
  resetSpeedTracker();
  state.isConnecting = true;
  setProgressState("uploading");
  applyActiveTransferBadge("upload");
  showProgressSpeed(t("正在建立 SSH 连接…"));

  await invoke("start_upload", { request: transferPayload(forceOverwrite) });
  return "started";
}


export function setTransferDirection(direction: TransferDirection) {
  if (state.transferDirection === direction) return;
  state.transferDirection = direction;
  const radio = document.querySelector<HTMLInputElement>(
    `input[name="transferDirection"][value="${direction}"]`,
  );
  if (radio) radio.checked = true;
  updateTransferDirectionUI();
}


/** 凭证里的校验方式说明，用于没有逐文件校验摘要的场景（如批量传输）。 */
export function verificationModeText(): string {
  return isReliableMode()
    ? t("弱网可靠模式：每块写入后读回比对 SHA-256，必要时再做整体 SHA-256 比对")
    : t("快速模式：传完后整体比对 SHA-256");
}


/** 完成后留给分块地图播放绿色波纹的时间，略长于 BlockMap 的波纹时长。 */
export const COMPLETE_REVEAL_MS = 1100;


/** 必须在 setTransferring(false) / resetSpeedTracker 之前取，之后计数会被清零。 */
export function collectRunStats(): RunStats {
  return {
    elapsedMs: state.runStartedAt > 0 ? Date.now() - state.runStartedAt : 0,
    movedBytes: Math.max(0, state.progressTransferredBytes - (state.runStartBytes ?? state.progressTransferredBytes)),
    retries: Math.max(state.retryCount, blockMap.retryCount),
  };
}


/** 传输成功后的收尾：有命令就先执行命令，再按命令结果决定结果页是「完成」还是「警告」。 */
export async function finishSuccessfulRun(
  title: string,
  message: string,
  stats: RunStats,
  remotePaths: string[],
  files: ReceiptFile[] = [{ local: state.runLocalPath, remote: state.runRemotePath }],
  verification = verifySummary.textContent || verificationModeText(),
) {
  // 走到这里说明文件都已送达且校验通过（命令失败不影响这一点），可以出具凭证。
  recordReceipt(state.runDirection, files, stats, verification);
  if (!state.runPostCommandTemplate) {
    showCommandResult(null);
    showResult("ok", title, message, stats);
    return;
  }
  const { outcome, error } = await runPostCommand(state.runPostCommandTemplate, commandContextFor(remotePaths));
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
export function showResult(
  kind: ResultKind,
  title: string,
  subtitle: string,
  stats: RunStats,
  transferOk = kind === "ok" || kind === "warn",
) {
  resultCard.className = `result-card ${kind}`;
  resultCard.dataset.transferOk = transferOk ? "1" : "";
  syncReceiptVisibility(transferOk);
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


export function handleProgressUpdate(
  transferred: number,
  total: number,
  status: string,
  message: string,
  retry_count: number,
  direction: TransferDirection,
  verify_summary?: string | null,
  doneRanges?: ByteRange[],
) {
  state.retryCount = retry_count;

  if (
    status !== "completed" &&
    status !== "cancelled" &&
    status !== "failed" &&
    status !== "cancelling"
  ) {
    state.isConnecting = false;
  }

  const uploading = direction === "upload";

  if (status === "stalled") {
    state.isReconnecting = true;
    progressWrap.classList.add("stalled");
    applyActiveTransferBadge(direction);
    showWaitingSpeedText(
      state.lastProgressEventAt > 0 ? Date.now() - state.lastProgressEventAt : UI_STALL_WARN_MS,
    );
    updateProgressDetail(transferred, total, retry_count);
    showTransferStatus(message, "retry");
    refreshLiveVisuals();
    return;
  }

  // 批量中的文件：历史与断点由队列在整批结束时统一刷新（见 finishQueue），
  // 每个文件都重新读一遍历史和断点文件，几百个文件就是几百次无用的往返。
  const inQueue = Boolean(state.resolveQueueStep);

  if (status === "completed") {
    updateProgress(transferred, total, doneRanges);
    const stats = collectRunStats();
    // 单文件完成后还有一段完成动画才进结果页（或开始执行命令），这期间仍算忙碌；
    // 必须在 setTransferring(false) 之前置位，否则表单会在动画期间被放开。
    if (!inQueue) state.finishingRun = true;
    setTransferring(false);
    setProgressState("idle");
    resetSpeedTracker();
    setBadge(t("已完成"), "verifying");
    statusText.textContent = "";
    // 批量的文件之间保留取消按钮：此时点它是取消整个批量剩下的文件。
    if (!inQueue) cancelBtn.classList.add("hidden");
    blockMap.setPhase("completed");
    showVerifySummary(
      verify_summary ??
        (isReliableMode()
          ? t("校验通过 · 逐块校验 + SHA-256 一致")
          : t("校验通过 · SHA-256 一致")),
    );
    if (uploading) state.activeUploadCheckpoint = null;
    else state.activeDownloadCheckpoint = null;
    updateTransferStatePanel();
    // 队列中的单个文件完成：交回调度器继续下一个，不展示单文件结果页。
    if (state.resolveQueueStep) {
      state.resolveQueueStep({ status: "completed", message, stats });
      return;
    }
    void loadHistory();
    // 先让地图播完「全部变绿」的波纹再切结果页：这一刻是整个等待的回报，值得被看到。
    window.setTimeout(() => {
      state.finishingRun = false;
      if (!state.isTransferring) {
        // 有传完后命令时，这里会同步进入「命令执行中」，下面的解锁判断仍是忙碌。
        const title = uploading ? t("上传完成") : t("下载完成");
        // 副标题写「什么传到了哪里」；后端消息只有带额外信息（如重试次数）时才值得再显示一遍。
        const route = `${fileNameOf(state.runRemotePath)} → ${
          uploading ? `${hostInput.value.trim()}:${state.runRemotePath}` : state.runLocalPath
        }`;
        const subtitle = message && message !== title ? `${route}\n${message}` : route;
        void finishSuccessfulRun(title, subtitle, stats, [state.runRemotePath]);
      }
      syncBusyLock();
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
    if (state.resolveQueueStep) {
      // 批量中不回填表单：断点里存的是当前这一个文件的完整路径，回填会把用户填的目标目录
      // 换成文件路径，之后「继续传输」剩余文件就全传进以文件名命名的目录里。
      state.resolveQueueStep({ status: "cancelled", message, stats });
      return;
    }
    void restoreActiveCheckpoint();
    void loadHistory();
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
    if (state.resolveQueueStep) {
      state.resolveQueueStep({ status: "failed", message, stats });
      return;
    }
    void refreshCheckpointFromDisk();
    void loadHistory();
    showResult("error", uploading ? t("上传失败") : t("下载失败"), clarifyTransferError(message), stats);
    return;
  }

  if (status === "verifying") {
    state.isChunkVerifying = false;
    state.isFullVerifying = true;
    markProgressActivity();
    setProgressState("verifying");
    setBadge(t("校验中"), "verifying");
    updateProgress(transferred, total, doneRanges);
    updateProgressDetail(transferred, total, retry_count);
    showTransferStatus(t("正在校验文件完整性，请稍候…"));
    showProgressSpeed(t("校验中"));
    refreshLiveVisuals();
    return;
  }

  if (status === "chunk_verifying") {
    // 块读回校验对用户透明，外观与正常传输一致，仅刷新心跳避免误报 stall
    state.isChunkVerifying = true;
    markProgressActivity();
    applyActiveTransferBadge(direction);
    updateProgress(transferred, total, doneRanges);
    updateProgressDetail(transferred, total, retry_count);
    hideTransferStatus();
    refreshLiveVisuals();
    return;
  }

  state.isChunkVerifying = false;
  state.isFullVerifying = false;

  if (status === "retrying") {
    state.isReconnecting = true;
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
  updateProgress(transferred, total, doneRanges);
  updateProgressDetail(transferred, total, retry_count);
  refreshSpeedDisplay();
  hideTransferStatus();
  refreshLiveVisuals();
}

