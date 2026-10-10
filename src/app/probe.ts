// 传输前的远端探测与预检（续传位置、空间、相同文件），结果显示在表单下方。
import { t } from "../i18n";
import { invoke } from "@tauri-apps/api/core";
import { connectionPayload, validateConnectionBeforeTransfer } from "./connection";
import { PROBE_DEBOUNCE_MS } from "./constants";
import { showConfirm } from "./dialogs";
import { clearCheckpointBtn, forceOverwriteInput, localPathInput, remotePathInput, statePanelDetail, statePanelHint, statePanelTitle, transferStatePanel } from "./dom";
import { formatBytes } from "./format";
import { renderPendingTransfers } from "./history";
import { isQueueSelection } from "./paths";
import { state } from "./state";
import { getActiveCheckpoint, getTransferredBytes, isDownloadMode } from "./transfer";
import { DownloadCheckpoint, RemoteDownloadProbe, RemoteUploadProbe, TransferStateView, UploadCheckpoint } from "./types";

export function analyzeCheckpointState(
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


export function analyzeProbeState(
  probe: RemoteUploadProbe | RemoteDownloadProbe,
  download: boolean,
): TransferStateView {
  if (forceOverwriteInput.checked) {
    return {
      kind: "warn",
      title: download ? t("将强制覆盖本地文件") : t("将强制覆盖远端文件"),
      detail: probe.message,
      // 上传先写临时文件：从头重传期间远端原文件不动，校验通过后才替换。
      hint: download
        ? t("开始传输前会删除已有文件并从头重来")
        : t("将从头重新上传，远端原文件在新文件校验通过后才会被替换"),
      showClear: false,
    };
  }

  const action = probe.action;
  if (action === "identical") {
    return {
      kind: "info",
      title: download ? t("本地已是最新") : t("远端已是最新"),
      detail: probe.message,
      hint: download
        ? t("开始后将直接完成，不会重新下载；如需重下请勾选「强制从头覆盖」")
        : t("开始后将直接完成，不会重新上传；如需重传请勾选「强制从头覆盖」"),
      showClear: false,
    };
  }
  if (action === "insufficient_space") {
    return {
      kind: "error",
      title: download ? t("本地磁盘空间不足") : t("远端磁盘空间不足"),
      detail: probe.message,
      hint: download
        ? t("请清理本地空间或换一个保存位置后再试")
        : t("请清理服务器空间或换一个目录后再试"),
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
      hint: action === "full_redownload"
        ? t("继续将删除已有文件并重新传输")
        : t("继续将从头重新上传，远端原文件在新文件校验通过前保持不变"),
      showClear: false,
    };
  }
  if (action === "overwrite") {
    return {
      kind: "warn",
      title: download ? t("将覆盖本地文件") : t("将替换远端文件"),
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


export function renderTransferStatePanel(view: TransferStateView) {
  transferStatePanel.classList.remove("hidden", "info", "warn", "error", "neutral");
  transferStatePanel.classList.add(view.kind);
  statePanelTitle.textContent = view.title;
  statePanelDetail.textContent = view.detail ?? "";
  statePanelDetail.classList.toggle("hidden", !view.detail);
  statePanelHint.textContent = view.hint ?? "";
  statePanelHint.classList.toggle("hidden", !view.hint);
  clearCheckpointBtn.classList.toggle("hidden", !view.showClear);
}


export function hideTransferStatePanel() {
  transferStatePanel.classList.add("hidden");
  clearCheckpointBtn.classList.add("hidden");
}


export function updateTransferStatePanel() {
  renderPendingTransfers();
  if (state.isTransferring) {
    hideTransferStatePanel();
    return;
  }

  const cp = getActiveCheckpoint();
  if (cp && cp.status !== "completed") {
    renderTransferStatePanel(analyzeCheckpointState(cp, isDownloadMode()));
    return;
  }

  if (state.lastProbe) {
    renderTransferStatePanel(analyzeProbeState(state.lastProbe, isDownloadMode()));
    return;
  }

  hideTransferStatePanel();
}


export function setLastProbe(probe: RemoteUploadProbe | RemoteDownloadProbe | null) {
  state.lastProbe = probe;
  updateTransferStatePanel();
}


export function clearLastProbe() {
  state.lastProbe = null;
  updateTransferStatePanel();
}


export function showDownloadProbeResult(probe: RemoteDownloadProbe) {
  setLastProbe(probe);
}


export function showProbeResult(probe: RemoteUploadProbe) {
  setLastProbe(probe);
}


export function clearProbeResult() {
  clearLastProbe();
}


/** 用户正在编辑路径时静默丢弃预检结果，不触发窗口自适应 */
export function invalidateProbeOnEdit() {
  if (!state.lastProbe) return;
  state.lastProbe = null;
  hideTransferStatePanel();
}


export async function probeRemoteDownload(): Promise<RemoteDownloadProbe> {
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


export async function confirmDownloadAction(probe: RemoteDownloadProbe): Promise<boolean> {
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


export async function probeRemoteUpload(): Promise<RemoteUploadProbe> {
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


export async function confirmUploadAction(probe: RemoteUploadProbe): Promise<boolean> {
  showProbeResult(probe);

  if (probe.action === "verify_retry" || probe.action === "resume" || probe.action === "new") {
    return true;
  }

  if (forceOverwriteInput.checked) {
    return showConfirm(
      t("强制覆盖上传"),
      t("将从头重新上传，远端原文件在新文件校验通过后才会被替换。是否继续？"),
    );
  }

  if (probe.action === "full_reupload") {
    return showConfirm(
      t("需要从头重传"),
      t("{message}\n\n将从头重新上传，是否继续？", { message: probe.message }),
    );
  }

  if (probe.action === "overwrite") {
    return showConfirm(
      t("替换远端文件"),
      t("{message}\n\n远端已有同名文件，是否上传并替换它？", { message: probe.message }),
    );
  }

  return true;
}


export function clearProbeDebounce() {
  if (state.probeDebounceTimer !== null) {
    window.clearTimeout(state.probeDebounceTimer);
    state.probeDebounceTimer = null;
  }
}


export function scheduleBackgroundProbe() {
  if (state.isTransferring) return;
  clearProbeDebounce();
  state.probeDebounceTimer = window.setTimeout(() => {
    state.probeDebounceTimer = null;
    void runBackgroundProbe();
  }, PROBE_DEBOUNCE_MS);
}


export async function runBackgroundProbe() {
  if (state.isTransferring || isQueueSelection()) return;
  const localPath = localPathInput.value.trim();
  const remotePath = remotePathInput.value.trim();
  if (!localPath || !remotePath) return;

  const gen = ++state.backgroundProbeGeneration;

  try {
    validateConnectionBeforeTransfer();
    const probe = isDownloadMode()
      ? await probeRemoteDownload()
      : await probeRemoteUpload();
    if (gen !== state.backgroundProbeGeneration || state.isTransferring) return;
    setLastProbe(probe);
  } catch {
    // 预检失败时不打扰用户，点开始时会再报
  }
}

