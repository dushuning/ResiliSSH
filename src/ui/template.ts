import { t } from "../i18n";
import { icons } from "./icons";

/**
 * 应用骨架：侧边栏导航 + 多页面。每个页面固定在一屏内，不再靠折叠和滚动容纳功能——
 * 新功能加一个页面/侧边栏项即可，不会挤压传输主流程。
 * 元素 id 与 main.ts 中的逻辑一一对应，修改时需同步。
 */
export function appTemplate(appName: string): string {
  return `
  <div class="app-shell">
    <aside class="sidebar" data-tauri-drag-region>
      <div class="sidebar-titlebar" data-tauri-drag-region></div>
      <div class="sidebar-brand" data-tauri-drag-region>
        <img class="sidebar-logo" src="/app-logo.png" alt="" width="26" height="26" />
        <span>${appName}</span>
      </div>
      <nav class="sidebar-nav" aria-label="${t("主导航")}">
        <button class="nav-item active" type="button" data-page="transfer" title="${t("传输 (⌘1)")}">
          ${icons.transfer}<span class="nav-label">${t("传输")}</span>
          <span class="nav-live hidden" id="navTransferLive" aria-label="${t("传输进行中")}"></span>
        </button>
        <button class="nav-item" type="button" data-page="servers" title="${t("服务器 (⌘2)")}">
          ${icons.server}<span class="nav-label">${t("服务器")}</span>
          <span class="nav-count hidden" id="navServerCount"></span>
        </button>
        <button class="nav-item" type="button" data-page="history" title="${t("历史 (⌘3)")}">
          ${icons.history}<span class="nav-label">${t("历史")}</span>
          <span class="nav-badge hidden" id="navHistoryBadge" title="${t("未完成的传输")}"></span>
        </button>
        <button class="nav-item" type="button" data-page="logs" title="${t("日志 (⌘4)")}">
          ${icons.logs}<span class="nav-label">${t("日志")}</span>
          <span class="nav-badge nav-badge-error hidden" id="navLogBadge" title="${t("未查看的错误")}"></span>
        </button>
        <button class="nav-item" type="button" data-page="settings" title="${t("设置 (⌘5)")}">
          ${icons.settings}<span class="nav-label">${t("设置")}</span>
        </button>
      </nav>
      <div class="sidebar-fill" data-tauri-drag-region></div>
      <button class="mini-transfer hidden" id="miniTransfer" type="button" title="${t("查看传输")}">
        <div class="mini-transfer-row">
          <span class="mini-transfer-label" id="miniTransferLabel">${t("上传中")}</span>
          <span class="mini-transfer-pct" id="miniTransferPct">0%</span>
        </div>
        <div class="mini-transfer-track"><div class="mini-transfer-fill" id="miniTransferFill"></div></div>
        <div class="mini-transfer-speed" id="miniTransferSpeed"></div>
      </button>
    </aside>

    <main class="content">
      <!-- ───────── 传输 ───────── -->
      <section class="page" id="page-transfer" data-page="transfer">
        <header class="page-header" data-tauri-drag-region>
          <h1>${t("传输")}</h1>
          <div class="segmented" role="radiogroup" aria-label="${t("传输方向")}">
            <label><input type="radio" name="transferDirection" value="upload" checked /><span>${icons.upload}${t("上传")}</span></label>
            <label><input type="radio" name="transferDirection" value="download" /><span>${icons.download}${t("下载")}</span></label>
          </div>
        </header>

        <div class="page-body">
          <!-- 编辑视图：填写本次传输 -->
          <div class="view" id="composeView">
            <div class="server-picker" id="serverPicker">
              <span class="server-picker-icon">${icons.server}</span>
              <div class="server-picker-main">
                <select id="profileSelect" aria-label="${t("服务器")}"></select>
                <span class="server-picker-summary" id="connectionSummary"></span>
              </div>
              <button class="btn btn-plain" id="editServerBtn" type="button">${t("编辑")}${icons.chevronRight}</button>
            </div>

            <div class="onboarding hidden" id="onboarding">
              <div class="onboarding-title">${t("先添加一台服务器")}</div>
              <div class="onboarding-detail" id="onboardingDetail"></div>
              <div class="onboarding-actions" id="onboardingActions"></div>
            </div>

            <div id="transferStatePanel" class="state-panel hidden">
              <div class="state-panel-main">
                <div class="state-panel-title" id="statePanelTitle"></div>
                <div class="state-panel-detail" id="statePanelDetail"></div>
                <div class="state-panel-hint" id="statePanelHint"></div>
              </div>
              <button class="btn btn-link hidden" id="clearCheckpointBtn" type="button">${t("清除断点")}</button>
            </div>

            <div class="field">
              <div class="field-label"><label for="localPath" id="localPathLabel">${t("本地文件")}</label></div>
              <div id="fileChip" class="file-chip hidden">
                <span class="file-chip-icon">${icons.file}</span>
                <div class="file-chip-body">
                  <div class="file-chip-name" id="fileChipName"></div>
                  <div class="file-chip-path" id="fileChipPath"></div>
                </div>
                <button class="btn btn-secondary btn-small" id="changeFileBtn" type="button">${t("更换")}</button>
              </div>
              <div class="drop-zone" id="dropZone">
                <div class="input-row">
                  <input id="localPath" placeholder="${t("选择或拖拽文件到此处")}" spellcheck="false" />
                  <button class="btn btn-secondary" id="pickFileBtn" type="button">${t("选择…")}</button>
                  <button class="btn btn-secondary" id="pickFolderBtn" type="button">${t("文件夹…")}</button>
                </div>
                <p class="drop-hint" id="dropHint">${t("也可以直接把文件拖到窗口里")}</p>
              </div>
            </div>

            <div class="field">
              <div class="field-label">
                <label for="remotePath" id="remotePathLabel">${t("远端路径")}</label>
                <span class="field-hint" id="pathHint"></span>
              </div>
              <div class="path-field">
                <div class="input-row">
                  <div class="path-input-wrap">
                    <input id="remotePath" placeholder="/home/user/dir/" spellcheck="false" />
                    <button class="path-star" id="bookmarkBtn" type="button" title="${t("收藏此目录")}">${icons.star}</button>
                  </div>
                  <button class="btn btn-secondary" id="pathMenuBtn" type="button" aria-haspopup="menu">${t("常用")}${icons.chevronDown}</button>
                  <button class="btn btn-secondary" id="browseRemoteBtn" type="button">${t("浏览…")}</button>
                </div>
                <div class="popover hidden" id="pathMenu" role="menu" aria-label="${t("常用目录")}"></div>
              </div>
              <p id="resolvedPathHint" class="resolved-path hidden"></p>
            </div>

            <div class="options-row">
              <div class="segmented segmented-small" role="radiogroup" aria-label="${t("传输模式")}">
                <label><input type="radio" name="transferMode" value="reliable" checked /><span>${t("弱网可靠")}</span></label>
                <label><input type="radio" name="transferMode" value="fast" /><span>${t("快速")}</span></label>
              </div>
              <span class="options-hint" id="modeHint"></span>
              <label class="checkbox">
                <input type="checkbox" id="forceOverwrite" />
                <span id="forceOverwriteLabel">${t("强制从头覆盖")}</span>
              </label>
            </div>

            <div class="path-field" id="commandField">
              <div class="input-row">
                <label class="command-label" for="postCommand">${icons.terminal}${t("传完后执行")}</label>
                <div class="path-input-wrap">
                  <input id="postCommand" placeholder="${t("可选，如 sudo systemctl restart app")}" spellcheck="false" autocomplete="off" />
                  <button class="path-star" id="saveCommandBtn" type="button" title="${t("保存为常用命令")}">${icons.star}</button>
                </div>
                <button class="btn btn-secondary" id="commandMenuBtn" type="button" aria-haspopup="menu">${t("常用")}${icons.chevronDown}</button>
              </div>
              <div class="popover popover-up hidden" id="commandMenu" role="menu" aria-label="${t("常用命令")}"></div>
            </div>
          </div>

          <!-- 进行中视图：分块地图 + 实时状态 -->
          <div class="view hidden" id="runView">
            <div class="progress-wrap" id="progressWrap">
              <div class="run-head" id="progressToolbar">
                <span class="queue-chip hidden" id="queueChip"></span>
                <div class="run-summary" id="runSummary"></div>
                <span id="progressBadge" class="badge">${t("等待开始")}</span>
              </div>
              <div class="map-frame">
                <canvas class="block-map" id="blockMap" aria-label="${t("分块传输进度")}"></canvas>
              </div>
              <div class="map-legend" id="mapLegend">
                <span class="hidden" id="legendResumed"><i class="swatch swatch-resumed"></i>${t("之前已续传")}</span>
                <span><i class="swatch swatch-done"></i><span id="legendDoneLabel">${t("本次已传")}</span></span>
                <span><i class="swatch swatch-active"></i>${t("正在传输")}</span>
                <span><i class="swatch swatch-pending"></i>${t("待传")}</span>
                <span class="hidden" id="legendRetry"><i class="swatch swatch-retry"></i>${t("断线重连处")}</span>
                <span class="map-caption" id="mapCaption"></span>
              </div>
              <div class="command-console hidden" id="commandConsole">
                <div class="command-console-head">
                  ${icons.terminal}<span class="command-console-cmd" id="commandConsoleCmd"></span>
                </div>
                <pre class="command-console-output" id="commandOutput"></pre>
              </div>
              <div class="run-stats" id="progressStats">
                <div class="run-percent" id="progressPercent">0%</div>
                <div class="run-stats-side">
                  <div class="progress-detail" id="progressDetail"></div>
                  <div class="progress-speed-line" id="progressSpeed"></div>
                </div>
              </div>
            </div>
          </div>

          <!-- 结果视图：完成 / 失败 / 取消 -->
          <div class="view hidden" id="resultView">
            <div class="result-card" id="resultCard">
              <div class="result-icon" id="resultIcon"></div>
              <div class="result-title" id="resultTitle"></div>
              <div class="result-subtitle" id="resultSubtitle"></div>
              <div class="verify-summary hidden" id="verifySummary"></div>
              <dl class="result-stats" id="resultStats"></dl>
              <div class="result-receipt hidden" id="resultReceipt">
                <button class="btn btn-plain btn-small" id="copyReceiptBtn" type="button">${t("复制传输凭证")}</button>
                <button class="btn btn-plain btn-small" id="saveReceiptBtn" type="button">${t("保存凭证…")}</button>
              </div>
              <ul class="result-files hidden" id="resultFiles"></ul>
              <div class="result-command hidden" id="resultCommand">
                <div class="result-command-line" id="resultCommandLine"></div>
                <pre class="result-command-output" id="resultCommandOutput"></pre>
              </div>
            </div>
          </div>
        </div>

        <footer class="page-footer">
          <div class="status" id="statusText"></div>
          <div class="footer-actions">
            <button class="btn btn-plain hidden" id="viewLogBtn" type="button">${icons.logs}${t("查看日志")}</button>
            <button class="btn btn-secondary hidden" id="resultBackBtn" type="button">${t("返回修改")}</button>
            <button class="btn btn-danger-soft hidden" id="cancelBtn" type="button">${t("取消传输")}</button>
            <button class="btn btn-primary btn-large" id="transferBtn" type="button">${t("开始上传")}</button>
          </div>
        </footer>
      </section>

      <!-- ───────── 服务器 ───────── -->
      <section class="page hidden" id="page-servers" data-page="servers">
        <header class="page-header" data-tauri-drag-region>
          <h1>${t("服务器")}</h1>
          <button class="btn btn-secondary btn-small" id="newServerBtn" type="button">${icons.plus}${t("新建")}</button>
        </header>
        <div class="page-body servers-layout">
          <div class="server-list-pane">
            <div class="server-list" id="serverList" role="listbox" aria-label="${t("已保存的服务器")}"></div>
            <div class="import-box">
              <label for="hostSelect">${t("从 ~/.ssh/config 导入")}</label>
              <select id="hostSelect">
                <option value="">${t("选择 Host…")}</option>
              </select>
            </div>
          </div>

          <form class="server-editor" id="serverEditor" autocomplete="off" onsubmit="return false">
            <div class="form-grid">
              <label for="profileName">${t("名称")}</label>
              <input id="profileName" placeholder="${t("例如 甘肃服务器")}" />

              <label for="host">${t("主机")}</label>
              <div class="host-port">
                <input id="host" placeholder="192.168.1.10" spellcheck="false" />
                <span class="host-port-sep">:</span>
                <input id="port" type="number" value="22" aria-label="${t("端口")}" />
              </div>

              <label for="username">${t("用户名")}</label>
              <input id="username" placeholder="root" spellcheck="false" />

              <label for="authType">${t("认证方式")}</label>
              <select id="authType">
                <option value="key">${t("私钥")}</option>
                <option value="password">${t("密码")}</option>
              </select>

              <div class="form-row-group" id="keyFields">
                <label for="keyPath">${t("私钥")}</label>
                <div class="input-row">
                  <input id="keyPath" placeholder="~/.ssh/id_ed25519" spellcheck="false" />
                  <button class="btn btn-secondary" id="pickKeyBtn" type="button">${t("选择…")}</button>
                </div>
                <label for="keyPassphrase">${t("私钥口令")}</label>
                <input id="keyPassphrase" type="password" placeholder="${t("没有可留空")}" />
              </div>

              <div class="form-row-group hidden" id="passwordFields">
                <label for="password">${t("密码")}</label>
                <input id="password" type="password" />
              </div>

              <label for="jumpProfile">${t("经由")}</label>
              <select id="jumpProfile" title="${t("内网服务器只能经堡垒机访问时，选择作为跳板机的已保存服务器（相当于 ssh -J）")}">
                <option value="">${t("直接连接")}</option>
              </select>
            </div>
            <label class="checkbox remember-row" id="rememberRow">
              <input type="checkbox" id="rememberSecrets" checked />
              <span>${t("在钥匙串中记住密码与口令")}</span>
            </label>
            <p class="hint" id="authHint"></p>
            <div class="connection-status" id="connectionStatus"></div>
            <div class="editor-actions">
              <button class="btn btn-danger-text" id="deleteProfileBtn" type="button">${t("删除")}</button>
              <span class="editor-actions-fill"></span>
              <button class="btn btn-secondary" id="testConnBtn" type="button">${t("测试连接")}</button>
              <button class="btn btn-primary" id="saveProfileBtn" type="button">${t("保存")}</button>
            </div>
          </form>
        </div>
      </section>

      <!-- ───────── 历史 ───────── -->
      <section class="page hidden" id="page-history" data-page="history">
        <header class="page-header" data-tauri-drag-region>
          <h1>${t("历史")}</h1>
        </header>
        <div class="page-body history-layout">
          <section class="history-section" id="pendingSection">
            <h2>${t("未完成的传输")}</h2>
            <div class="pending-list" id="pendingList"></div>
          </section>
          <section class="history-section history-section-grow">
            <h2>${t("最近传输")}</h2>
            <div class="history-list" id="historyList"></div>
          </section>
        </div>
      </section>

      <!-- ───────── 日志 ───────── -->
      <section class="page hidden" id="page-logs" data-page="logs">
        <header class="page-header" data-tauri-drag-region>
          <h1>${t("日志")}</h1>
          <div class="header-tools">
            <div class="segmented segmented-small" role="radiogroup" aria-label="${t("日志级别")}">
              <label><input type="radio" name="logFilter" value="all" checked /><span>${t("全部")}</span></label>
              <label><input type="radio" name="logFilter" value="warn" /><span>${t("警告及以上")}</span></label>
              <label><input type="radio" name="logFilter" value="error" /><span>${t("仅错误")}</span></label>
            </div>
            <button class="btn btn-secondary btn-small" id="copyLogBtn" type="button">${icons.copy}${t("复制")}</button>
            <button class="btn btn-plain btn-small" id="clearLogBtn" type="button">${t("清空")}</button>
          </div>
        </header>
        <div class="page-body logs-layout">
          <div class="log-list" id="logList" role="log" aria-live="polite" aria-label="${t("活动日志")}"></div>
          <button class="log-jump hidden" id="logJumpBtn" type="button">${t("↓ 有新日志")}</button>
        </div>
      </section>

      <!-- ───────── 设置 ───────── -->
      <section class="page hidden" id="page-settings" data-page="settings">
        <header class="page-header" data-tauri-drag-region>
          <h1>${t("设置")}</h1>
        </header>
        <div class="page-body settings-layout">
          <section class="settings-group">
            <h2>${t("外观")}</h2>
            <div class="settings-row">
              <span>${t("主题")}</span>
              <div class="segmented segmented-small" role="radiogroup" aria-label="${t("主题")}">
                <label><input type="radio" name="themeMode" value="system" checked /><span>${t("跟随系统")}</span></label>
                <label><input type="radio" name="themeMode" value="light" /><span>${t("浅色")}</span></label>
                <label><input type="radio" name="themeMode" value="dark" /><span>${t("深色")}</span></label>
              </div>
            </div>
          </section>
          <section class="settings-group">
            <h2>${t("语言")}</h2>
            <div class="settings-row">
              <span>${t("界面语言")}</span>
              <div class="segmented segmented-small" role="radiogroup" aria-label="${t("界面语言")}">
                <label><input type="radio" name="languageMode" value="system" checked /><span>${t("跟随系统")}</span></label>
                <label><input type="radio" name="languageMode" value="zh" /><span>中文</span></label>
                <label><input type="radio" name="languageMode" value="en" /><span>English</span></label>
              </div>
            </div>
            <p class="hint" id="languageHint"></p>
          </section>
          <section class="settings-group">
            <h2>${t("数据与安全")}</h2>
            <div class="settings-row"><span>${t("密码与口令")}</span><span class="settings-value">${t("macOS 钥匙串")}</span></div>
            <div class="settings-row"><span>${t("已信任的服务器指纹")}</span><code class="settings-value">~/Library/Application Support/sshutil/known_hosts</code></div>
            <div class="settings-row"><span>${t("断点与历史")}</span><code class="settings-value">~/Library/Application Support/sshutil/</code></div>
            <div class="settings-row"><span>${t("诊断日志")}</span><code class="settings-value">~/Library/Logs/com.resiliss.desktop/</code></div>
          </section>
          <section class="settings-group">
            <h2>${t("关于")}</h2>
            <div class="settings-row"><span>${appName}</span><span class="settings-value" id="appVersion"></span></div>
            <p class="hint">${t("弱网环境下的单文件 SFTP 传输：断了接着传，传完自动校验。")}</p>
          </section>
        </div>
      </section>
    </main>
  </div>

  <div id="confirmOverlay" class="modal-overlay hidden">
    <div class="modal" role="dialog" aria-modal="true" aria-labelledby="confirmTitle" aria-describedby="confirmBody">
      <div class="modal-title" id="confirmTitle">${t("确认")}</div>
      <div class="modal-body" id="confirmBody"></div>
      <div class="modal-actions">
        <button class="btn btn-secondary" id="confirmCancel" type="button">${t("取消")}</button>
        <button class="btn btn-primary" id="confirmOk" type="button">${t("确定")}</button>
      </div>
    </div>
  </div>

  <div id="remoteBrowseOverlay" class="modal-overlay hidden">
    <div class="modal modal-browse" role="dialog" aria-modal="true" aria-labelledby="remoteBrowseTitle">
      <div class="modal-title" id="remoteBrowseTitle">${t("浏览远端目录")}</div>
      <div class="input-row">
        <input id="remoteBrowsePath" type="text" spellcheck="false" />
        <button class="btn btn-secondary btn-small" id="remoteBrowseGoBtn" type="button">${t("前往")}</button>
      </div>
      <div class="remote-browse-status" id="remoteBrowseStatus"></div>
      <div class="remote-browse-hint-row">
        <p class="hint" id="remoteBrowseHint">${t("双击文件夹进入；双击文件可选中")}</p>
        <label class="checkbox remote-browse-hidden-toggle">
          <input type="checkbox" id="remoteBrowseShowHidden" />
          <span>${t("显示隐藏文件")}</span>
        </label>
      </div>
      <div class="remote-browse-list" id="remoteBrowseList"></div>
      <div class="input-row remote-browse-new-dir hidden" id="remoteBrowseNewDirRow">
        <input id="remoteBrowseNewDirName" type="text" placeholder="${t("新目录名称")}" spellcheck="false" />
        <button class="btn btn-secondary btn-small" id="remoteBrowseCreateDirBtn" type="button">${t("创建")}</button>
        <button class="btn btn-plain btn-small" id="remoteBrowseCancelNewDirBtn" type="button">${t("取消")}</button>
      </div>
      <div class="modal-actions">
        <button class="btn btn-secondary" id="remoteBrowseUpBtn" type="button">${t("上级目录")}</button>
        <button class="btn btn-secondary" id="remoteBrowseNewDirBtn" type="button">${t("新建目录")}</button>
        <span class="editor-actions-fill"></span>
        <button class="btn btn-secondary" id="remoteBrowseSelectDirBtn" type="button">${t("选择此目录")}</button>
        <button class="btn btn-primary" id="remoteBrowseCloseBtn" type="button">${t("关闭")}</button>
      </div>
    </div>
  </div>
`;
}
