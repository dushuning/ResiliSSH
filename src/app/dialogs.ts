// 应用内确认对话框：替代 window.confirm，样式统一且可 await。
import { t } from "../i18n";
import { CONFIRM_ANIM_MS } from "./constants";
import { icons } from "../ui/icons";
import { confirmBody, confirmCancel, confirmIcon, confirmOk, confirmOverlay, confirmTitle } from "./dom";
import { state } from "./state";

export function openConfirmDialog() {
  state.confirmClosing = false;
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


/** 用户对确认框的回答：点了主按钮、点了取消按钮，或没有作答就关掉了（Esc / 点遮罩）。 */
export type ConfirmChoice = "ok" | "cancel" | "dismissed";

export interface ConfirmOptions {
  /** 主按钮文案，写成动作本身（「上传并替换」「删除」），而不是笼统的「确定」。 */
  ok?: string;
  cancel?: string;
  /** danger：不可撤销或会丢进度的操作，主按钮用红色；warn：有代价但可恢复。 */
  tone?: "default" | "warn" | "danger";
}

/** 只关心「是否确认」时用这个；需要区分「明确拒绝」与「没作答」时用 showConfirmChoice。 */
export async function showConfirm(title: string, body: string, options: ConfirmOptions = {}): Promise<boolean> {
  return (await showConfirmChoice(title, body, options)) === "ok";
}

export function showConfirmChoice(
  title: string,
  body: string,
  options: ConfirmOptions = {},
): Promise<ConfirmChoice> {
  // 上一个确认框还在播放关闭动画：立刻按用户当时的选择了结它并撤掉定时器，
  // 否则定时器到点会把这个新弹出的确认框隐藏掉，并拿旧答案去回答新问题。
  const pending = state.confirmPendingClose;
  if (pending) {
    window.clearTimeout(pending.timer);
    state.confirmPendingClose = null;
    pending.settle();
  }
  if (state.confirmResolver) {
    const stale = state.confirmResolver;
    state.confirmResolver = null;
    stale("dismissed");
  }

  confirmTitle.textContent = title;
  confirmBody.textContent = body;
  // 按钮文案默认「确定 / 取消」；需要更明确的动作时（如「继续传输 / 稍后」）由调用方指定。
  confirmOk.textContent = options.ok ?? t("确定");
  confirmCancel.textContent = options.cancel ?? t("取消");
  const tone = options.tone ?? "default";
  confirmOverlay.dataset.tone = tone;
  confirmIcon.innerHTML = tone === "default" ? icons.info : icons.alert;
  // 危险操作的主按钮用红色，避免和其他确认框长得一样、被习惯性地点掉。
  confirmOk.classList.toggle("btn-danger", tone === "danger");
  confirmOk.classList.toggle("btn-primary", tone !== "danger");
  openConfirmDialog();

  return new Promise((resolve) => {
    state.confirmResolver = resolve;
  });
}


export function closeConfirm(result: ConfirmChoice) {
  if (state.confirmClosing || confirmOverlay.classList.contains("hidden")) {
    return;
  }

  state.confirmClosing = true;
  confirmOk.disabled = true;
  confirmCancel.disabled = true;
  confirmOverlay.classList.remove("is-visible");

  // 点击的这一刻就取走 resolver：答案属于当前这个确认框，不能等动画结束后再去读
  // state.confirmResolver——那时它可能已经属于下一个确认框。
  const resolve = state.confirmResolver;
  state.confirmResolver = null;
  const settle = () => resolve?.(result);
  const timer = window.setTimeout(() => {
    state.confirmPendingClose = null;
    confirmOverlay.classList.add("hidden");
    state.confirmClosing = false;
    settle();
  }, CONFIRM_ANIM_MS);
  state.confirmPendingClose = { timer, settle };
}
