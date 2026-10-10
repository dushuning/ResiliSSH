// 传输凭证：把「送到了，而且两端一致」整理成一份可复制、可保存的文本，便于贴进工单、发给同事或留档。
import { t } from "../i18n";
import { copyText } from "../ui/activityLog";
import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { connectionPayload } from "./connection";
import { copyReceiptBtn, resultReceipt, saveReceiptBtn } from "./dom";
import { describeError, formatBytes, formatDuration } from "./format";
import { setStatus } from "./progress";
import { state } from "./state";
import { ReceiptFile, RunReceipt, RunStats, TransferDirection } from "./types";

/** 复制成功后按钮上的「已复制」提示停留多久。 */
const COPIED_FEEDBACK_MS = 1500;

interface FileDigest {
  size: number;
  sha256: string;
}

/** 成功收尾时调用：记下本次传输，结果页随之显示凭证按钮。 */
export function recordReceipt(
  direction: TransferDirection,
  files: ReceiptFile[],
  stats: RunStats,
  verification: string,
) {
  const payload = connectionPayload();
  const via: string[] = [];
  // 从最外层的跳板机写起，与实际连接顺序一致。
  for (let hop = payload.jump; hop; hop = hop.jump) via.unshift(`${hop.username}@${hop.host}:${hop.port}`);
  const finishedAt = Date.now();
  state.lastReceipt = {
    direction,
    endpoint: `${payload.username}@${payload.host}:${payload.port}`,
    via,
    startedAt: finishedAt - stats.elapsedMs,
    finishedAt,
    elapsedMs: stats.elapsedMs,
    retries: stats.retries,
    verification,
    files,
  };
}


/** 新一轮传输开始或结果不是完整成功时清掉，避免把上一次的凭证当成这一次的。 */
export function clearReceipt() {
  state.lastReceipt = null;
  resultReceipt.classList.add("hidden");
}


export function syncReceiptVisibility(transferOk: boolean) {
  resultReceipt.classList.toggle("hidden", !(transferOk && state.lastReceipt));
}


function pad(n: number): string {
  return String(n).padStart(2, "0");
}


/** 带时区偏移的本地时间：凭证可能被发给其他时区的同事，裸时间会产生歧义。 */
function formatTimestamp(ms: number): string {
  const d = new Date(ms);
  const offset = -d.getTimezoneOffset();
  const sign = offset >= 0 ? "+" : "-";
  const zone = `${sign}${pad(Math.floor(Math.abs(offset) / 60))}:${pad(Math.abs(offset) % 60)}`;
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())} ${zone}`
  );
}


async function buildReceiptText(receipt: RunReceipt): Promise<string> {
  const digests = await invoke<(FileDigest | null)[]>("local_file_digests", {
    paths: receipt.files.map((f) => f.local),
  });
  const download = receipt.direction === "download";
  const server = receipt.via.length > 0
    ? t("{endpoint}（经 {via}）", { endpoint: receipt.endpoint, via: receipt.via.join(" → ") })
    : receipt.endpoint;
  const lines = [
    t("ResiliSSH 传输凭证"),
    "",
    `${t("方向")}：${download ? t("下载") : t("上传")}`,
    `${t("目标服务器")}：${server}`,
    `${t("开始时间")}：${formatTimestamp(receipt.startedAt)}`,
    `${t("完成时间")}：${formatTimestamp(receipt.finishedAt)}（${formatDuration(receipt.elapsedMs)}）`,
    `${t("校验方式")}：${receipt.verification}`,
    `${t("自动重连")}：${t("{retries} 次", { retries: receipt.retries })}`,
    "",
    t("文件（{count} 个）：", { count: receipt.files.length }),
  ];
  receipt.files.forEach((file, i) => {
    const digest = digests[i];
    lines.push(
      "",
      `  ${t("本地")}：${file.local}`,
      `  ${t("远端")}：${file.remote}`,
      `  ${t("大小")}：${digest ? `${formatBytes(digest.size)}（${digest.size} B）` : t("无法读取")}`,
      `  SHA-256：${digest?.sha256 ?? t("无法读取（本地文件可能已被移动或修改）")}`,
    );
    if (file.skipped) lines.push(`  ${t("说明")}：${t("两端已完全一致，本次未传输")}`);
  });
  lines.push(
    "",
    t("SHA-256 为本地文件的哈希；传输时已与服务器上的文件逐块或整体比对一致。"),
    t("在服务器上可用 sha256sum <文件> 自行核对。"),
  );
  return lines.join("\n") + "\n";
}


export async function copyReceipt() {
  const receipt = state.lastReceipt;
  if (!receipt) return;
  const label = t("复制传输凭证");
  copyReceiptBtn.disabled = true;
  copyReceiptBtn.textContent = t("正在计算 SHA-256…");
  try {
    await copyText(await buildReceiptText(receipt));
    copyReceiptBtn.textContent = t("已复制");
    window.setTimeout(() => {
      copyReceiptBtn.textContent = label;
    }, COPIED_FEEDBACK_MS);
  } catch (error) {
    copyReceiptBtn.textContent = label;
    setStatus(describeError(error), "error");
  } finally {
    copyReceiptBtn.disabled = false;
  }
}


export async function saveReceipt() {
  const receipt = state.lastReceipt;
  if (!receipt) return;
  const d = new Date(receipt.finishedAt);
  const stamp = `${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}-${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}`;
  const path = await save({
    defaultPath: `ResiliSSH-receipt-${stamp}.txt`,
    filters: [{ name: t("文本文件"), extensions: ["txt"] }],
  });
  if (!path) return;
  saveReceiptBtn.disabled = true;
  try {
    await invoke("save_text_file", { path, content: await buildReceiptText(receipt) });
    setStatus(t("凭证已保存到 {path}", { path }));
  } catch (error) {
    setStatus(describeError(error), "error");
  } finally {
    saveReceiptBtn.disabled = false;
  }
}
