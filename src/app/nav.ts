// 侧边栏页面切换。
import { activityLog, miniTransfer } from "./dom";
import { state } from "./state";
import { PageId } from "./types";

/** 页面切换只改显隐，不重建 DOM：传输进行中切走再切回，进度与地图状态都保持原样。 */
export function showPage(page: PageId) {
  state.currentPage = page;
  document.querySelectorAll<HTMLElement>(".page").forEach((el) => {
    el.classList.toggle("hidden", el.dataset.page !== page);
  });
  document.querySelectorAll<HTMLButtonElement>(".nav-item").forEach((el) => {
    const active = el.dataset.page === page;
    el.classList.toggle("active", active);
    if (active) el.setAttribute("aria-current", "page");
    else el.removeAttribute("aria-current");
  });
  activityLog.setVisible(page === "logs");
  // 迷你进度只在离开传输页时有意义；在传输页上它与大进度重复。
  miniTransfer.classList.toggle("hidden", !state.isTransferring || page === "transfer");
}

