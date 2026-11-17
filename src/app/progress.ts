// 传输进度展示：进度条、速度估算、分块地图与迷你进度条的刷新。
import { t } from "../i18n";
import { type BlockMapPhase } from "../ui/blockMap";
import { icons } from "../ui/icons";
import { clarifyTransferError } from "./backendText";
import { SPEED_REFRESH_MS, SPEED_SAMPLE_WINDOW_MS, SPEED_STALE_MS, UI_STALL_WARN_MS } from "./constants";
import { blockMap, hostInput, legendDoneLabel, legendResumed, legendRetry, localPathInput, mapCaption, mapLegend, miniTransfer, miniTransferFill, miniTransferLabel, miniTransferPct, miniTransferSpeed, navTransferLive, progressBadge, progressDetail, progressPercent, progressSpeed, progressWrap, queueChip, remotePathInput, runSummary, statusText, verifySummary } from "./dom";
import { fileNameOf, floorPercent, formatBytes, formatEtaSeconds, formatProgressPercent } from "./format";
import { renderQueueResultFiles } from "./queue";
import { updateDockProgress } from "./dock";
import { clearReceipt } from "./receipt";
import { state } from "./state";
import { isDownloadMode, isReliableMode } from "./transfer";
import { ByteRange, TransferDirection } from "./types";

const MS_PER_SECOND = 1000;

/** 状态行里并列的几段之间的分隔，与「大小 · 速度 · 剩余时间」的写法一致。 */
const STATUS_SEPARATOR = " · ";

/** 侧边栏迷你进度的那行文字。多数时候与进行中视图的速度行相同，连接出问题时是它的短版本。 */
let miniSpeedText = "";

/** 进入进行中视图前重置分块地图，并写好「谁 → 哪里」的摘要行。 */
export function beginRunVisuals() {
  state.progressTotalBytes = 0;
  state.progressTransferredBytes = 0;
  state.runStartedAt = Date.now();
  state.runStartBytes = null;
  clearReceipt();
  blockMap.reset();
  blockMap.setVerifiedPerBlock(isReliableMode());
  if (state.activeQueue) {
    updateQueueChip(null);
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
  kind: "idle" | "uploading" | "retrying" | "verifying" | "stalled" = "idle",
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


/** 样式上只有「重试中」需要区别对待（百分比变色），其余阶段外观相同，不必各挂一个类。 */
export function setProgressState(state: "idle" | "uploading" | "retrying" | "verifying") {
  progressWrap.classList.toggle("retrying", state === "retrying");
}


/**
 * 传输进行中徽章：连接中 / 重连中 / 无响应 / 正常传输。
 * 「重连中」排在「无响应」前面：重连拖得久了看门狗也会判定为无响应，但此时应用已经在处理，
 * 徽章若跳回「无响应」就和下面那行「正在自动重连」互相矛盾。
 */
export function applyActiveTransferBadge(direction: TransferDirection) {
  const badgeActive = direction === "upload" ? t("上传中") : t("下载中");
  if (state.isConnecting) {
    setProgressState("uploading");
    setBadge(t("连接中"), "retrying");
    return;
  }
  if (state.isReconnecting) {
    setProgressState("retrying");
    setBadge(t("重连中"), "retrying");
    return;
  }
  if (progressWrap.classList.contains("stalled")) {
    setProgressState("retrying");
    setBadge(t("无响应"), "stalled");
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
  // 文字用统一的向下取整；填充条用精确比例，移动才平滑。
  miniTransferPct.textContent = `${floorPercent(state.progressTransferredBytes, state.progressTotalBytes)}%`;
  const fill = state.progressTotalBytes > 0 ? (state.progressTransferredBytes / state.progressTotalBytes) * 100 : 0;
  miniTransferFill.style.width = `${fill}%`;
  syncProgressStatsRow();
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
  // 断线时用户最担心的是「前面传的是不是白费了」：这一行就地回答它，重连次数此时由下一行给出。
  if (state.isReconnecting) return t("{size} · 已传部分已保存，重连后自动续传", { size });
  return retries > 0 ? t("{size} · 已自动重连 {retries} 次", { size, retries }) : size;
}


export function syncProgressStatsRow() {
  // 进行中视图的统计行固定显示（避免布局跳动），这里只同步侧边栏迷你进度的速度文字。
  miniTransferSpeed.textContent = miniSpeedText;
}


/** brief：侧边栏迷你进度里显示的短版本，不给时与主文案相同。 */
export function showProgressSpeed(text: string, stale = false, brief = text) {
  progressSpeed.textContent = text;
  progressSpeed.classList.remove("hidden");
  progressSpeed.classList.toggle("stale", stale);
  miniSpeedText = brief;
  syncProgressStatsRow();
}


/** 距下一次重连尝试还剩几秒；0 表示退避已结束（正在连接），或后端没有给出退避时间。 */
function retryCountdownSecs(): number {
  if (state.retryScheduledAt <= 0 || state.retryDelayMs <= 0) return 0;
  return Math.max(0, Math.ceil((state.retryScheduledAt + state.retryDelayMs - Date.now()) / MS_PER_SECOND));
}


/**
 * 连接出问题期间的副文本，依次回答三件事：出了什么事、应用正在自己做什么、现在是第几次。
 * 同一时刻只有一个数字在走（倒计时，或已断开的秒数），既能看出界面没有卡死，又不至于眼花。
 * brief 给侧边栏的迷你进度用：那里只有一行很窄的位置，而「重连中」的标签已经交代了出了什么事，
 * 只需要最关键的那一段。
 */
function linkTroubleText(): { full: string; brief: string } {
  const secs = Math.floor(Math.max(0, Date.now() - state.lastProgressEventAt) / MS_PER_SECOND);
  // 还没有重连事件：连接没断，只是迟迟没有数据。后端静默超过它自己的阈值会主动断开重连，这里先交代这一点。
  if (!state.isReconnecting) {
    const silent = t("已 {secs} 秒没有响应", { secs });
    return { full: [silent, t("持续无响应将自动重连")].join(STATUS_SEPARATOR), brief: silent };
  }
  const remaining = retryCountdownSecs();
  const action = remaining > 0 ? t("{remaining} 秒后自动重连", { remaining }) : t("正在自动重连…");
  const parts = [remaining > 0 ? t("连接中断") : t("连接已中断 {secs} 秒", { secs }), action];
  if (state.retryCount > 0) parts.push(t("第 {attempt} 次", { attempt: state.retryCount }));
  return { full: parts.join(STATUS_SEPARATOR), brief: action };
}


/**
 * 进入或刷新「连接有问题」的呈现：徽章、副文本、地图阶段和侧边栏迷你进度一次同步。
 * 重连事件、后端的 stalled / retrying 状态、看门狗的每秒刷新都走这里，
 * 否则各处各写一套文案，徽章说「等待」而副文本说「重连」这类自相矛盾就会出现。
 */
export function showLinkTrouble() {
  applyActiveTransferBadge(state.runDirection);
  const text = linkTroubleText();
  showProgressSpeed(text.full, true, text.brief);
  // 卡住时没有可信的速度，总进度里的剩余时间也必须去掉，否则会停在卡住前的乐观数字上。
  updateQueueChip(null);
  refreshLiveVisuals();
}


export function hideProgressSpeed() {
  progressSpeed.classList.add("hidden");
  progressSpeed.classList.remove("stale");
  progressSpeed.textContent = "";
  miniSpeedText = "";
  syncProgressStatsRow();
}


export function updateProgressDetail(transferred: number, total: number, retries = state.retryCount) {
  if (total > 0 && state.isTransferring) {
    progressDetail.textContent = formatProgressDetail(transferred, total, retries);
    progressDetail.classList.remove("hidden");
  } else if (!state.isTransferring) {
    progressDetail.classList.add("hidden");
  }
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
 * transferred 是已传字节总数，驱动百分比和 Dock 进度；哪些块已完成由 doneRanges 决定
 * （并行传输时不连续），没有区间的事件由地图按「开头连续的一段」处理。
 * 这里只更新进度数字与地图；字节明细和侧边栏等「活的」指示由调用方在处理完整个事件后
 * 各刷新一次（见 handleProgressUpdate），否则一个事件会把同样的文字重写好几遍。
 */
export function updateProgress(transferred: number, total: number, doneRanges?: ByteRange[]) {
  // 只在传输进行中驱动地图：空闲时恢复断点也会调用这里，不能把断点进度当成本次起点。
  if (!state.isTransferring) return;
  state.progressTotalBytes = total;
  state.progressTransferredBytes = transferred;
  if (state.runStartBytes === null) state.runStartBytes = transferred;
  progressPercent.textContent = formatProgressPercent(transferred, total) || "0%";
  updateDockProgress(transferred, total);
  blockMap.update(transferred, total, doneRanges);
  const perCell = blockMap.blocksPerCell;
  mapCaption.textContent =
    perCell > 1
      ? t("{p1} 块 · 每格 {perCell} MB", { p1: blockMap.blockCount.toLocaleString(), perCell })
      : t("{p1} 块 · 每块 1 MB", { p1: blockMap.blockCount.toLocaleString() });
  syncMapLegend();
}


/** 图例只列出地图上真实出现的状态：全新上传没有「此前已完成」，没断过线就不提「重连处」。 */
export function syncMapLegend() {
  const perBlock = blockMap.verifiesPerBlock;
  mapLegend.classList.toggle("per-block", perBlock);
  legendDoneLabel.textContent = perBlock ? t("本次已传并校验") : t("本次已传（待最终校验）");
  legendResumed.classList.toggle("hidden", !blockMap.hasResumedPart);
  // 「重连处」只藏不撤（保留占位）：第一次断线时图例若多出一项而折行，地图会被挤矮一截，
  // 偏偏发生在最需要画面稳定的时刻。它排在右对齐的说明文字之前，占位不会留下可见的空隙。
  legendRetry.classList.toggle("is-reserved", blockMap.retryCount === 0);
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
    // 速度要攒够两个采样才算得出来；这之前先明确告诉用户已经接上了，而不是让「重连中」的旧文案多留一拍。
    // 整体校验阶段不写：那里有自己的状态文案。
    if (!state.isFullVerifying) showProgressSpeed(t("已重新连接，继续传输"));
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


/**
 * 批量传输的总进度：只看「第 3 / 238 个」无法知道整体还要多久。剩余时间按当前速度估算，
 * 与单文件的估算一致；队列里有文件大小未知时只显示个数。
 */
export function updateQueueChip(speedBps: number | null) {
  const queue = state.activeQueue;
  if (!queue) {
    queueChip.classList.add("hidden");
    return;
  }
  const parts = [t("第 {p1} / {length} 个", { p1: queue.index + 1, length: queue.items.length })];
  if (queue.totalBytes > 0) {
    const processed = Math.min(queue.totalBytes, queue.doneBytes + state.progressTransferredBytes);
    parts.push(`${formatBytes(processed)} / ${formatBytes(queue.totalBytes)}`);
    if (speedBps && speedBps > 0 && processed < queue.totalBytes) {
      const eta = formatEtaSeconds((queue.totalBytes - processed) / speedBps);
      if (eta) parts.push(t("全部约 {eta}", { eta }));
    }
  }
  queueChip.textContent = parts.join(" · ");
  queueChip.classList.remove("hidden");
}


export function refreshSpeedDisplay() {
  if (!state.isTransferring) return;
  // 重连和整体校验期间，这一行各有自己的文案（见 showLinkTrouble / refreshStallUi）：采样窗口里还留着
  // 之前的数据，不拦住的话每秒都会把「正在自动重连」「校验中」盖回一个已经不成立的速度。
  if (state.isReconnecting || state.isFullVerifying) return;

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
    return;
  }

  updateQueueChip(speed);
  let speedText = `${formatBytes(speed)}/s`;
  const transferred = state.speedSamples[state.speedSamples.length - 1]?.bytes ?? 0;
  const total = state.progressTotalBytes;
  if (total > transferred && speed > 0) {
    const eta = formatEtaSeconds((total - transferred) / speed);
    if (eta) speedText += t(" · 约 {eta}", { eta });
  }
  // 侧边栏的速度文字已由 showProgressSpeed 同步，不需要再整体刷新一遍活动指示。
  showProgressSpeed(speedText);
}


export function refreshStallUi() {
  if (!state.isTransferring) return;

  // 连接阶段要先于下面的「尚无进度事件」判断：此时本来就没有进度事件，
  // 但弱网下握手可能要十几秒，必须让用户看到等待时间在走，而不是一行静止的文字。
  if (state.isConnecting) {
    applyActiveTransferBadge(isDownloadMode() ? "download" : "upload");
    const secs = Math.floor((Date.now() - state.runStartedAt) / MS_PER_SECOND);
    showProgressSpeed(`${t("正在建立 SSH 连接…")} (${t("已等待 {secs}s", { secs })})`);
    refreshLiveVisuals();
    return;
  }

  if (state.lastProgressEventAt === 0) return;

  if (state.isChunkVerifying) {
    return;
  }

  // 整体校验期间后端只在开始时发一次事件，之后要等远端算完整个文件的哈希才有下文。
  // 这段沉默是正常的，不能当成「无响应」；改为显示已校验多久，让用户看到它在走。
  if (state.isFullVerifying) {
    const secs = Math.floor((Date.now() - state.lastProgressEventAt) / MS_PER_SECOND);
    showProgressSpeed(`${t("校验中")} (${t("已等待 {secs}s", { secs })})`);
    return;
  }

  const elapsed = Date.now() - state.lastProgressEventAt;
  // 重连中每秒都刷新（倒计时在走）；没有重连事件时，沉默超过阈值才提示。
  if (elapsed < SPEED_STALE_MS && !state.isReconnecting) return;

  if (elapsed >= UI_STALL_WARN_MS) progressWrap.classList.add("stalled");
  showLinkTrouble();
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

