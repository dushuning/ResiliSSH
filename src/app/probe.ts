// 传输前的远端探测与预检（续传位置、空间、相同文件），结果显示在表单下方。
import { t } from "../i18n";
import { invoke } from "@tauri-apps/api/core";
import { connectionPayload, validateConnectionBeforeTransfer } from "./connection";
import { PROBE_DEBOUNCE_MS } from "./constants";
import { showConfirm } from "./dialogs";
import { clearCheckpointBtn, forceOverwriteInput, jumpProfileSelect, localPathInput, remotePathInput, statePanelDetail, statePanelHint, statePanelTitle, transferStatePanel } from "./dom";
import { floorPercent, formatBytes } from "./format";
import { renderPendingTransfers } from "./history";
import { checkpointMatchesForm, isQueueSelection } from "./paths";
import { isBusy, state } from "./state";
import { getActiveCheckpoint, getTransferredBytes, isDownloadMode } from "./transfer";
import { DownloadCheckpoint, RemoteDownloadProbe, RemoteUploadProbe, TransferStateView, UploadCheckpoint } from "./types";

export function analyzeCheckpointState(
  cp: UploadCheckpoint | DownloadCheckpoint,
  download: boolean,
): TransferStateView {
  const transferred = getTransferredBytes(cp);
  const total = cp.file_size;
  const pct = floorPercent(transferred, total);
  const action = download ? t("开始下载") : t("开始上传");
  const verifyReadFailed = cp.failure_reason === "verify_read_failed";
  const needsRedo =
    cp.failure_reason === "hash_mismatch" ||
    // total 为 0 的空文件「已传 >= 总量」恒成立，不能据此断定校验失败。
    (cp.status === "failed" && total > 0 && transferred >= total && !verifyReadFailed);

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
      // 两个方向都先写临时文件：从头重传期间原文件不动，校验通过后才替换。
      hint: download
        ? t("将从头重新下载，本地原文件在新文件校验通过后才会被替换")
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
        ? t("继续将从头重新下载，本地原文件在新文件校验通过前保持不变")
        : t("继续将从头重新上传，远端原文件在新文件校验通过前保持不变"),
      showClear: false,
    };
  }
  if (action === "overwrite") {
    return {
      kind: "warn",
      title: download ? t("将替换本地文件") : t("将替换远端文件"),
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

  // 断点只有与当前表单指向同一个文件、同一台服务器时后端才会续传；
  // 表单已改成别的目标时还显示「可续传 40%」是在承诺一件不会发生的事。
  const cp = getActiveCheckpoint();
  if (cp && cp.status !== "completed" && checkpointMatchesForm(cp, isDownloadMode())) {
    renderTransferStatePanel(analyzeCheckpointState(cp, isDownloadMode()));
    return;
  }

  if (state.lastProbe) {
    renderTransferStatePanel(analyzeProbeState(state.lastProbe, isDownloadMode()));
    return;
  }

  hideTransferStatePanel();
}


/**
 * 一次预检的目标：方向 + 服务器 + 两端路径。用于判断「这个问题是否已经问过 / 正在问」，
 * 不含密码等凭据——凭据不影响预检结论，失败的预检也不会被记住。
 */
function currentProbeKey(): string {
  const { host, port, username } = connectionPayload();
  return JSON.stringify([
    state.transferDirection,
    host,
    port,
    username,
    jumpProfileSelect.value,
    localPathInput.value.trim(),
    remotePathInput.value.trim(),
  ]);
}


/** 调用时表单必须仍是发起预检时的内容（开始流程中表单已锁定；后台预检由代次保证），这样记下的目标才与结果对应。 */
export function setLastProbe(probe: RemoteUploadProbe | RemoteDownloadProbe | null) {
  state.lastProbe = probe;
  state.lastProbeKey = probe ? currentProbeKey() : null;
  updateTransferStatePanel();
}


export function clearLastProbe() {
  // 同时作废在途的后台预检：否则改了路径或切了方向之后，旧请求的结果还会被写回面板。
  state.backgroundProbeGeneration++;
  state.lastProbe = null;
  state.lastProbeKey = null;
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


/**
 * 用户正在编辑路径：丢弃预检结果，并作废在途的后台预检（即使当前没有结果在显示——
 * 否则针对旧路径的请求稍后返回，会把过时的结论写回面板）。
 * 面板要重新判断而不是直接隐藏：路径改回与断点一致时，断点状态应当重新出现。
 */
export function invalidateProbeOnEdit() {
  clearLastProbe();
}


export async function probeRemoteDownload(): Promise<RemoteDownloadProbe> {
  validateConnectionBeforeTransfer();
  const localPath = localPathInput.value.trim();
  const remotePath = remotePathInput.value.trim();
  if (!localPath) throw new Error(t("请填写本地保存路径"));
  if (!remotePath) throw new Error(t("请填写远端文件路径"));

  return sharedProbe<RemoteDownloadProbe>("probe_remote_download", localPath, remotePath);
}


export async function confirmDownloadAction(probe: RemoteDownloadProbe): Promise<boolean> {
  showDownloadProbeResult(probe);
  if (state.activeQueue) return true;

  if (probe.action === "verify_retry" || probe.action === "resume" || probe.action === "new") {
    return true;
  }

  if (forceOverwriteInput.checked) {
    return showConfirm(
      t("强制覆盖下载"),
      t("将从头重新下载，本地原文件在新文件校验通过后才会被替换。是否继续？"),
    );
  }

  if (probe.action === "full_redownload") {
    return showConfirm(
      t("需要从头重下"),
      t("{message}\n\n将从头重新下载，是否继续？", { message: probe.message }),
    );
  }

  if (probe.action === "overwrite") {
    return showConfirm(
      t("替换本地文件"),
      t("{message}\n\n本地已有同名文件，是否下载并替换它？", { message: probe.message }),
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

  return sharedProbe<RemoteUploadProbe>("probe_remote_upload", localPath, remotePath);
}


/**
 * 发出预检请求；目标相同的请求已在途时直接复用它。
 * 典型场景：点「开始」时路径框失焦触发了后台预检，紧接着开始流程又要预检同一个目标——
 * 每次预检都是一条完整的 SSH 连接，弱网下重复一次就是多等几秒。
 * 目标里包含方向，所以复用到的结果类型一定与 T 一致。
 */
function sharedProbe<T extends RemoteUploadProbe | RemoteDownloadProbe>(
  command: "probe_remote_upload" | "probe_remote_download",
  localPath: string,
  remotePath: string,
): Promise<T> {
  const key = currentProbeKey();
  if (state.inflightProbe?.key === key) return state.inflightProbe.promise as Promise<T>;

  const promise = invoke<T>(command, {
    request: {
      ...connectionPayload(),
      local_path: localPath,
      remote_path: remotePath,
    },
  });
  const entry = { key, promise };
  state.inflightProbe = entry;
  // 成功失败都要让位；只清自己这一条，免得误清之后发出的另一个请求。
  const release = () => {
    if (state.inflightProbe === entry) state.inflightProbe = null;
  };
  promise.then(release, release);
  return promise;
}


export async function confirmUploadAction(probe: RemoteUploadProbe): Promise<boolean> {
  showProbeResult(probe);
  // 批量传输已在开始前统一确认过（见 confirmBatch），逐个文件不再询问。
  if (state.activeQueue) return true;

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
  if (isBusy()) return;
  clearProbeDebounce();
  state.probeDebounceTimer = window.setTimeout(() => {
    state.probeDebounceTimer = null;
    void runBackgroundProbe();
  }, PROBE_DEBOUNCE_MS);
}


export async function runBackgroundProbe() {
  // 开始流程自己会预检（见 startOneUpload / startOneDownload），此时后台再发一次只是多开一条连接。
  if (isBusy() || isQueueSelection()) return;
  const localPath = localPathInput.value.trim();
  const remotePath = remotePathInput.value.trim();
  if (!localPath || !remotePath) return;
  // 结论还显示着、表单也没变：只是焦点进出了一次输入框，不值得再连一次服务器。
  if (state.lastProbe && state.lastProbeKey === currentProbeKey()) return;

  const gen = ++state.backgroundProbeGeneration;

  try {
    validateConnectionBeforeTransfer();
    const probe = isDownloadMode()
      ? await probeRemoteDownload()
      : await probeRemoteUpload();
    if (gen !== state.backgroundProbeGeneration || isBusy()) return;
    setLastProbe(probe);
  } catch {
    // 预检失败时不打扰用户，点开始时会再报
  }
}

