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

  window.setTimeout(() => {
    confirmOverlay.classList.add("hidden");
    state.confirmClosing = false;
    const resolve = state.confirmResolver;
    state.confirmResolver = null;
    resolve?.(result);
  }, CONFIRM_ANIM_MS);
}

