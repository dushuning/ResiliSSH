// 传输进度展示：进度条、速度估算、分块地图与迷你进度条的刷新。
import { t } from "../i18n";
import { type BlockMapPhase } from "../ui/blockMap";
import { icons } from "../ui/icons";
import { clarifyTransferError } from "./backendText";
import { SPEED_REFRESH_MS, SPEED_SAMPLE_WINDOW_MS, SPEED_STALE_MS, UI_STALL_WARN_MS } from "./constants";
import { blockMap, hostInput, legendDoneLabel, legendResumed, legendRetry, localPathInput, mapCaption, mapLegend, miniTransfer, miniTransferFill, miniTransferLabel, miniTransferPct, miniTransferSpeed, navTransferLive, progressBadge, progressDetail, progressPercent, progressSpeed, progressWrap, queueChip, remotePathInput, runSummary, statusText, verifySummary } from "./dom";
import { fileNameOf, formatBytes, formatEtaSeconds, formatProgressPercent } from "./format";
import { renderQueueResultFiles } from "./queue";
import { state } from "./state";
import { isDownloadMode, isReliableMode } from "./transfer";
import { TransferDirection } from "./types";

/** 进入进行中视图前重置分块地图，并写好「谁 → 哪里」的摘要行。 */
export function beginRunVisuals() {
  state.progressTotalBytes = 0;
  state.progressTransferredBytes = 0;
  state.runStartedAt = Date.now();
  state.runStartBytes = null;
  blockMap.reset();
  blockMap.setVerifiedPerBlock(isReliableMode());
  if (state.activeQueue) {
    queueChip.textContent = t("第 {p1} / {length} 个", { p1: state.activeQueue.index + 1, length: state.activeQueue.items.length });
    queueChip.classList.remove("hidden");
  } else {
    queueChip.classList.add("hidden");
    renderQueueResultFiles(null);
  }
  syncMapLegend();
  mapCaption.textContent = "";
  progressPercent.textContent = "0%";

  const download = state.runDirection === "download";
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


export function setBadge(
  text: string,
  kind: "idle" | "uploading" | "retrying" | "verifying" | "error" | "stalled" = "idle",
) {
  progressBadge.textContent = text;
  progressBadge.className = "badge";
  if (kind !== "idle") progressBadge.classList.add(kind);
}


export function setStatus(message: string, kind: "normal" | "error" | "retry" = "normal") {
  const text = kind === "error" ? clarifyTransferError(message) : message;
  statusText.textContent = text;
  statusText.classList.remove("error", "retry");
  if (kind === "error") statusText.classList.add("error");
  if (kind === "retry") statusText.classList.add("retry");
}


export function setProgressState(state: "idle" | "uploading" | "retrying" | "verifying") {
  progressWrap.classList.remove("uploading", "retrying", "verifying");
  if (state === "uploading") progressWrap.classList.add("uploading");
  if (state === "retrying") progressWrap.classList.add("retrying");
  if (state === "verifying") progressWrap.classList.add("verifying");
}


/** 传输进行中徽章：连接中 / 重连无响应 / 正常传输 */
export function applyActiveTransferBadge(direction: TransferDirection) {
  const badgeActive = direction === "upload" ? t("上传中") : t("下载中");
  if (state.isConnecting) {
    setProgressState("uploading");
    setBadge(t("连接中"), "retrying");
    return;
  }
  if (progressWrap.classList.contains("stalled")) {
    setProgressState("retrying");
    setBadge(t("等待响应"), "stalled");
    return;
  }
  if (state.isReconnecting) {
    setProgressState("retrying");
    setBadge(t("重试中"), "retrying");
    return;
  }
  setProgressState("uploading");
  setBadge(badgeActive, "uploading");
}


/** 当前传输所处阶段，驱动分块地图的动效与侧边栏迷你进度的配色。 */
export function resolveMapPhase(): BlockMapPhase {
  if (state.isConnecting) return "connecting";
  if (state.isFullVerifying) return "verifying";
  if (
    state.isReconnecting ||
    progressWrap.classList.contains("stalled") ||
    (state.lastProgressEventAt > 0 && Date.now() - state.lastProgressEventAt >= SPEED_STALE_MS)
  ) {
    return "waiting";
  }
  return "transferring";
}


/**
 * 同步所有「活的」指示：分块地图阶段、侧边栏迷你进度、导航上的进行中圆点。
 * 侧边栏迷你进度让用户切到其他页面时仍能看到传输状态，不必守在传输页。
 */
export function refreshLiveVisuals() {
  const phase = state.isTransferring ? resolveMapPhase() : null;
  if (phase) blockMap.setPhase(phase);

  const waiting = phase === "waiting" || phase === "connecting";
  navTransferLive.classList.toggle("hidden", !state.isTransferring);
  navTransferLive.classList.toggle("is-waiting", waiting);
  miniTransfer.classList.toggle("hidden", !state.isTransferring || state.currentPage === "transfer");
  miniTransfer.classList.toggle("is-waiting", waiting);
  if (!state.isTransferring) return;

  const verb = state.runDirection === "download" ? t("下载") : t("上传");
  const queuePart = state.activeQueue ? ` ${state.activeQueue.index + 1}/${state.activeQueue.items.length}` : "";
  miniTransferLabel.textContent =
    (phase === "connecting" ? t("连接中") : phase === "verifying" ? t("校验中") : waiting ? t("重连中") : t("{verb}中", { verb })) +
    queuePart;
  const pct = state.progressTotalBytes > 0 ? (state.progressTransferredBytes / state.progressTotalBytes) * 100 : 0;
  miniTransferPct.textContent = `${Math.floor(pct)}%`;
  miniTransferFill.style.width = `${pct}%`;
  miniTransferSpeed.textContent = progressSpeed.textContent ?? "";
}


export function estimateTransferSpeedBps(): number | null {
  if (state.speedSamples.length < 2) return null;
  const first = state.speedSamples[0];
  const last = state.speedSamples[state.speedSamples.length - 1];
  const deltaT = (last.t - first.t) / 1000;
  if (deltaT < 0.3) return null;
  const deltaBytes = last.bytes - first.bytes;
  if (deltaBytes <= 0) return 0;
  return deltaBytes / deltaT;
}


export function hideVerifySummary() {
  verifySummary.classList.add("hidden");
  verifySummary.textContent = "";
}


export function showVerifySummary(text: string) {
  verifySummary.textContent = text;
  verifySummary.classList.remove("hidden");
}


export function formatProgressDetail(transferred: number, total: number, retries: number): string {
  // 百分比已经用大号数字单独展示，这里只给字节数与重连次数。
  const size = `${formatBytes(transferred)} / ${formatBytes(total)}`;
  return retries > 0 ? t("{size} · 已自动重连 {retries} 次", { size, retries }) : size;
}


export function syncProgressStatsRow() {
  // 进行中视图的统计行固定显示（避免布局跳动），这里只同步侧边栏迷你进度的速度文字。
  miniTransferSpeed.textContent = progressSpeed.textContent ?? "";
}


export function showProgressSpeed(text: string, stale = false) {
  progressSpeed.textContent = text;
  progressSpeed.classList.remove("hidden");
  progressSpeed.classList.toggle("stale", stale);
  syncProgressStatsRow();
}


/** 重试退避剩余秒数，供副文本展示 */
export function formatRetryDelayHint(): string {
  if (state.retryScheduledAt <= 0 || state.retryDelayMs <= 0) return "";
  const remaining = Math.max(0, Math.ceil((state.retryScheduledAt + state.retryDelayMs - Date.now()) / 1000));
  return remaining > 0 ? t("约 {remaining}s 后重试", { remaining }) : "";
}


/** Stall/retry 副文本：动态计时，避免用户误以为界面卡死 */
export function showWaitingSpeedText(elapsedMs: number) {
  const secs = Math.floor(elapsedMs / 1000);
  const delayHint = formatRetryDelayHint();
  const details: string[] = [];
  if (state.isReconnecting && state.retryCount > 0) details.push(t("第 {retryCount} 次重试", { retryCount: state.retryCount }));
  if (delayHint) details.push(delayHint);
  details.push(t("已等待 {secs}s", { secs }));
  const prefix = state.isReconnecting ? t("重连中...") : t("等待响应...");
  showProgressSpeed(`${prefix} (${details.join(" · ")})`, true);
}


/** 刚进入重连时的副文本 */
export function showReconnectingSpeedText() {
  const delayHint = formatRetryDelayHint();
  const details: string[] = [];
  if (state.retryCount > 0) details.push(t("第 {retryCount} 次重试", { retryCount: state.retryCount }));
  if (delayHint) details.push(delayHint);
  const prefix = t("重连中...");
  showProgressSpeed(
    details.length > 0 ? `${prefix} (${details.join(" · ")})` : prefix,
    true,
  );
}


export function hideProgressSpeed() {
  progressSpeed.classList.add("hidden");
  progressSpeed.classList.remove("stale");
  progressSpeed.textContent = "";
  syncProgressStatsRow();
}


export function updateProgressDetail(transferred: number, total: number, retries = state.retryCount) {
  if (total > 0 && state.isTransferring) {
    progressDetail.textContent = formatProgressDetail(transferred, total, retries);
    progressDetail.classList.remove("hidden");
  } else if (!state.isTransferring) {
    progressDetail.classList.add("hidden");
  }
  syncProgressStatsRow();
}


export function showTransferStatus(message: string, kind: "normal" | "error" | "retry" = "normal") {
  statusText.classList.remove("hidden");
  setStatus(message, kind);
}


export function hideTransferStatus() {
  if (state.isTransferring) {
    statusText.classList.add("hidden");
    statusText.textContent = "";
  }
}


/**
 * 分块地图需要块内的细粒度进度（部分填充 + 当前块呼吸），所以每个进度事件都刷新；
 * 实际绘制由 BlockMap 按帧合并，事件再频繁也不会过度重绘。
 */
export function updateProgress(transferred: number, total: number) {
  // 只在传输进行中驱动地图：空闲时恢复断点也会调用这里，不能把断点进度当成本次起点。
  if (!state.isTransferring) return;
  state.progressTotalBytes = total;
  state.progressTransferredBytes = transferred;
  if (state.runStartBytes === null) state.runStartBytes = transferred;
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
export function syncMapLegend() {
  const perBlock = blockMap.verifiesPerBlock;
  mapLegend.classList.toggle("per-block", perBlock);
  legendDoneLabel.textContent = perBlock ? t("本次已传并校验") : t("本次已传（待最终校验）");
  legendResumed.classList.toggle("hidden", !blockMap.hasResumedPart);
  legendRetry.classList.toggle("hidden", blockMap.retryCount === 0);
}



export function resetSpeedTracker() {
  state.lastProgressEventAt = 0;
  state.speedSamples = [];
  state.isReconnecting = false;
  state.isConnecting = false;
  state.isChunkVerifying = false;
  state.isFullVerifying = false;
  state.retryDelayMs = 0;
  state.retryScheduledAt = 0;
  hideProgressSpeed();
  progressWrap.classList.remove("stalled");
  progressDetail.classList.add("hidden");
  syncProgressStatsRow();
}


export function markProgressActivity() {
  state.lastProgressEventAt = Date.now();
  state.isConnecting = false;
  state.isChunkVerifying = false;
  state.retryDelayMs = 0;
  state.retryScheduledAt = 0;
  const wasStalled = progressWrap.classList.contains("stalled");
  const wasReconnecting = state.isReconnecting;
  progressWrap.classList.remove("stalled");
  progressSpeed.classList.remove("stale");
  state.isReconnecting = false;
  if (state.isTransferring && (wasStalled || wasReconnecting)) {
    applyActiveTransferBadge(isDownloadMode() ? "download" : "upload");
    refreshLiveVisuals();
  }
}


export function recordSpeedSample(bytes: number) {
  markProgressActivity();
  const now = Date.now();
  state.speedSamples.push({ t: now, bytes });
  while (state.speedSamples.length > 0 && now - state.speedSamples[0].t > SPEED_SAMPLE_WINDOW_MS) {
    state.speedSamples.shift();
  }
}


export function refreshSpeedDisplay() {
  if (!state.isTransferring) return;

  const elapsed = Date.now() - state.lastProgressEventAt;
  if (elapsed >= SPEED_STALE_MS) {
    return;
  }

  if (state.speedSamples.length < 2) {
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
  const transferred = state.speedSamples[state.speedSamples.length - 1]?.bytes ?? 0;
  const total = state.progressTotalBytes;
  if (total > transferred && speed > 0) {
    const eta = formatEtaSeconds((total - transferred) / speed);
    if (eta) speedText += t(" · 约 {eta}", { eta });
  }
  showProgressSpeed(speedText);
  refreshLiveVisuals();
}


export function refreshStallUi() {
  if (!state.isTransferring || state.lastProgressEventAt === 0) return;

  if (state.isConnecting) {
    applyActiveTransferBadge(isDownloadMode() ? "download" : "upload");
    showProgressSpeed(t("正在建立 SSH 连接…"));
    refreshLiveVisuals();
    return;
  }

  if (state.isChunkVerifying) {
    return;
  }

  const elapsed = Date.now() - state.lastProgressEventAt;
  if (elapsed < SPEED_STALE_MS && !state.isReconnecting) return;

  if (elapsed >= SPEED_STALE_MS) {
    showWaitingSpeedText(elapsed);
    refreshLiveVisuals();
  }

  if (elapsed >= UI_STALL_WARN_MS) {
    progressWrap.classList.add("stalled");
    applyActiveTransferBadge(isDownloadMode() ? "download" : "upload");
  }
}


export function startProgressWatchdog() {
  stopProgressWatchdog();
  state.lastProgressEventAt = Date.now();
  state.progressWatchdogTimer = window.setInterval(refreshStallUi, SPEED_REFRESH_MS);
  state.speedDisplayTimer = window.setInterval(refreshSpeedDisplay, SPEED_REFRESH_MS);
}


export function stopProgressWatchdog() {
  if (state.progressWatchdogTimer !== null) {
    window.clearInterval(state.progressWatchdogTimer);
    state.progressWatchdogTimer = null;
  }
  if (state.speedDisplayTimer !== null) {
    window.clearInterval(state.speedDisplayTimer);
    state.speedDisplayTimer = null;
  }
}

