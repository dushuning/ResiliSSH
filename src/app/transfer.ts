// 单个文件的上传/下载流程：启动、进度事件处理、完成与失败的收尾。
import { t } from "../i18n";
import { icons } from "../ui/icons";
import { invoke } from "@tauri-apps/api/core";
import { BACKEND_TEXT, clarifyTransferError, matchesBackend } from "./backendText";
import { commandContextFor, describeCommandFailure, runPostCommand, showCommandResult } from "./command";
import { connectionPayload, prepareConnection } from "./connection";
import { TRANSFER_LOCK_SELECTORS, UI_STALL_WARN_MS } from "./constants";
import { blockMap, browseRemoteBtn, cancelBtn, composeView, dropHint, dropZone, forceOverwriteInput, forceOverwriteLabel, localPathInput, localPathLabel, modeHint, pathHint, pickFolderBtn, postCommandInput, progressDetail, progressWrap, remotePathInput, remotePathLabel, resultBackBtn, resultCard, resultCommand, resultIcon, resultStats, resultSubtitle, resultTitle, resultView, runView, statusText, testConnBtn, transferBtn, verifySummary, viewLogBtn } from "./dom";
import { describeError, formatBytes, formatDuration, prefersReducedMotion } from "./format";
import { loadHistory, refreshCheckpointFromDisk, restoreActiveCheckpoint } from "./history";
import { isQueueSelection, updateResolvedPathHint } from "./paths";
import { clearProbeDebounce, confirmDownloadAction, confirmUploadAction, probeRemoteDownload, probeRemoteUpload, showDownloadProbeResult, showProbeResult, updateTransferStatePanel } from "./probe";
import { applyActiveTransferBadge, beginRunVisuals, hideProgressSpeed, hideTransferStatus, hideVerifySummary, markProgressActivity, recordSpeedSample, refreshLiveVisuals, refreshSpeedDisplay, resetSpeedTracker, setBadge, setProgressState, showProgressSpeed, showReconnectingSpeedText, showTransferStatus, showVerifySummary, showWaitingSpeedText, startProgressWatchdog, stopProgressWatchdog, updateProgress, updateProgressDetail } from "./progress";
import { clearDockProgress } from "./dock";
import { recordReceipt, syncReceiptVisibility } from "./receipt";
import { updateRemoteBrowseModeUi } from "./remoteBrowse";
import { state } from "./state";
import { DownloadCheckpoint, ReceiptFile, ResultKind, RunStats, TransferDirection, TransferView, UploadCheckpoint, UploadStart } from "./types";

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


export function setTransferring(running: boolean) {
  state.isTransferring = running;
  // 批量中每个文件结束都会短暂置为 false，此时不清，免得 Dock 进度在文件之间闪烁；整批结束时由队列清除。
  if (!running && !state.activeQueue) clearDockProgress();
  transferBtn.disabled = running;
  testConnBtn.disabled = running;
  setTransferFormLocked(running);
  syncDirectionLock();
  cancelBtn.textContent = t("取消传输");
  cancelBtn.classList.remove("is-cancelling");
  if (running) {
    state.isConnecting = true;
    state.runDirection = state.transferDirection;
    clearProbeDebounce();
    state.backgroundProbeGeneration++;
    state.lastProbe = null;
    hideVerifySummary();
    statusText.textContent = "";
    beginRunVisuals();
    showTransferView("run");
    startProgressWatchdog();
  } else {
    state.cancelInFlight = false;
    cancelBtn.disabled = false;
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
  pickFolderBtn.classList.toggle("hidden", download);
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


export function isReliableMode(): boolean {
  const selected = document.querySelector<HTMLInputElement>(
    'input[name="transferMode"]:checked',
  );
  return selected?.value !== "fast";
}


export function updateModeHint() {
  modeHint.textContent = isReliableMode()
    ? t("每块写完读回比对，略慢但最稳")
    : t("只在传完后整体校验，速度更快");
}


export function uploadPayload(forceOverwrite: boolean) {
  return {
    ...connectionPayload(),
    local_path: localPathInput.value.trim(),
    remote_path: remotePathInput.value.trim(),
    strict_chunk_verify: isReliableMode(),
    force_overwrite: forceOverwrite,
    // 批量中的文件由队列结束时整体记一条历史，避免一次批量挤掉全部历史。
    in_batch: Boolean(state.activeQueue),
  };
}


export function downloadPayload(forceOverwrite: boolean) {
  return {
    ...connectionPayload(),
    local_path: localPathInput.value.trim(),
    remote_path: remotePathInput.value.trim(),
    strict_chunk_verify: isReliableMode(),
    force_overwrite: forceOverwrite,
    in_batch: Boolean(state.activeQueue),
  };
}


export async function invokeStartDownload(forceOverwrite: boolean) {
  try {
    await invoke("start_download", {
      request: downloadPayload(forceOverwrite),
    });
  } catch (error) {
    const message = describeError(error);
    if (!state.isTransferring && matchesBackend(message, BACKEND_TEXT.downloadRunning)) {
      await invoke("reset_download_state");
      await invoke("start_download", {
        request: downloadPayload(forceOverwrite),
      });
      return;
    }
    throw error;
  }
}


export async function startDownloadFlow() {
  await prepareConnection();
  const outcome = await startOneDownload();
  if (outcome === "identical") await finishIdenticalTransfer("download");
}


/** 预检、确认、启动一个下载；与 startOneUpload 对称，批量下载逐个调用它。 */
export async function startOneDownload(): Promise<UploadStart> {
  const probe = await probeRemoteDownload();
  // 本地磁盘不够时直接拦下，理由与上传相同。
  if (probe.action === "insufficient_space") {
    showDownloadProbeResult(probe);
    throw new Error(probe.message);
  }
  if (probe.action === "identical" && !forceOverwriteInput.checked) {
    showDownloadProbeResult(probe);
    state.runRemotePath = probe.resolved_remote_path;
    state.runLocalPath = probe.resolved_local_path;
    state.runPostCommandTemplate = postCommandInput.value.trim();
    return "identical";
  }
  const confirmed = await confirmDownloadAction(probe);
  if (!confirmed) return "declined";

  const forceOverwrite =
    probe.action === "full_redownload" || forceOverwriteInput.checked;
  state.runRemotePath = probe.resolved_remote_path;
  state.runLocalPath = probe.resolved_local_path;
  state.runPostCommandTemplate = postCommandInput.value.trim();

  setTransferring(true);
  state.retryCount = 0;
  resetSpeedTracker();
  state.isConnecting = true;
  setProgressState("uploading");
  applyActiveTransferBadge("download");
  showProgressSpeed(t("正在建立 SSH 连接…"));

  await invokeStartDownload(forceOverwrite);
  return "started";
}


export async function invokeStartUpload(forceOverwrite: boolean) {
  try {
    await invoke("start_upload", {
      request: uploadPayload(forceOverwrite),
    });
  } catch (error) {
    const message = describeError(error);
    if (!state.isTransferring && matchesBackend(message, BACKEND_TEXT.uploadRunning)) {
      await invoke("reset_upload_state");
      await invoke("start_upload", {
        request: uploadPayload(forceOverwrite),
      });
      return;
    }
    throw error;
  }
}


export async function startUploadFlow() {
  await prepareConnection();
  const outcome = await startOneUpload();
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

  await invokeStartUpload(forceOverwrite);
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
    if (uploading) state.activeUploadCheckpoint = null;
    else state.activeDownloadCheckpoint = null;
    updateTransferStatePanel();
    void loadHistory();
    // 队列中的单个文件完成：交回调度器继续下一个，不展示单文件结果页。
    if (state.resolveQueueStep) {
      state.resolveQueueStep({ status: "completed", message, stats });
      return;
    }
    // 先让地图播完「全部变绿」的波纹再切结果页：这一刻是整个等待的回报，值得被看到。
    window.setTimeout(() => {
      if (state.isTransferring) return;
      void finishSuccessfulRun(uploading ? t("上传完成") : t("下载完成"), message, stats, [state.runRemotePath]);
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
    if (state.resolveQueueStep) {
      state.resolveQueueStep({ status: "cancelled", message, stats });
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
    if (state.resolveQueueStep) {
      state.resolveQueueStep({ status: "failed", message, stats });
      return;
    }
    showResult("error", uploading ? t("上传失败") : t("下载失败"), clarifyTransferError(message), stats);
    return;
  }

  if (status === "verifying") {
    state.isChunkVerifying = false;
    state.isFullVerifying = true;
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
    state.isChunkVerifying = true;
    markProgressActivity();
    applyActiveTransferBadge(direction);
    updateProgress(transferred, total);
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
  updateProgress(transferred, total);
  updateProgressDetail(transferred, total, retry_count);
  refreshSpeedDisplay();
  hideTransferStatus();
  refreshLiveVisuals();
}

