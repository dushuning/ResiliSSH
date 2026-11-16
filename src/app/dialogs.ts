// 应用内确认对话框：替代 window.confirm，样式统一且可 await。
import { t } from "../i18n";
import { CONFIRM_ANIM_MS } from "./constants";
import { confirmBody, confirmCancel, confirmOk, confirmOverlay, confirmTitle } from "./dom";
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


export function showConfirm(
  title: string,
  body: string,
  labels: { ok?: string; cancel?: string } = {},
): Promise<boolean> {
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
    stale(false);
  }

  confirmTitle.textContent = title;
  confirmBody.textContent = body;
  // 按钮文案默认「确定 / 取消」；需要更明确的动作时（如「继续传输 / 稍后」）由调用方指定。
  confirmOk.textContent = labels.ok ?? t("确定");
  confirmCancel.textContent = labels.cancel ?? t("取消");
  openConfirmDialog();

  return new Promise((resolve) => {
    state.confirmResolver = resolve;
  });
}


export function closeConfirm(result: boolean) {
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
