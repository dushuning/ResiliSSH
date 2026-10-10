// Dock 图标上的进度条：大文件通常放在后台传，不切回窗口也能看到进度。
import { getCurrentWindow, ProgressBarStatus } from "@tauri-apps/api/window";
import { state } from "./state";

const NOT_SHOWN = -1;
/** 上次设置的整数百分比。进度事件很密集，只在百分比变化时才调用系统接口。 */
let lastPercent = NOT_SHOWN;


function setDockProgress(status: ProgressBarStatus, progress?: number) {
  getCurrentWindow()
    .setProgressBar({ status, progress })
    .catch((error) => console.warn("cannot update dock progress", error));
}


/** 批量时显示整体进度（与总进度标签一致），否则显示当前文件进度。 */
export function updateDockProgress(transferred: number, total: number) {
  const queue = state.activeQueue;
  const fraction =
    queue && queue.totalBytes > 0
      ? (queue.doneBytes + transferred) / queue.totalBytes
      : total > 0
        ? transferred / total
        : 0;
  const percent = Math.max(0, Math.min(100, Math.floor(fraction * 100)));
  if (percent === lastPercent) return;
  lastPercent = percent;
  setDockProgress(ProgressBarStatus.Normal, percent);
}


export function clearDockProgress() {
  if (lastPercent === NOT_SHOWN) return;
  lastPercent = NOT_SHOWN;
  setDockProgress(ProgressBarStatus.None);
}
