// 当前连接参数的读取与校验：表单 → 后端连接参数、钥匙串密码的会话缓存、连接状态展示。
import { t } from "../i18n";
import { invoke } from "@tauri-apps/api/core";
import { showConfirm } from "./dialogs";
import { authHint, authTypeSelect, connectionStatus, connectionSummary, hostInput, keyFields, keyPassphraseInput, keyPathInput, passwordFields, passwordInput, portInput, profileSelect, rememberRow, rememberSecretsInput, usernameInput } from "./dom";
import { updateBookmarkButton } from "./paths";
import { updateTransferStatePanel } from "./probe";
import { currentProfile, highlightSelectedServer } from "./servers";
import { ConnectionReadiness, HostKeyInfo, ProfileSecrets } from "./types";

export function getConnectionReadiness(): ConnectionReadiness {
  const host = hostInput.value.trim();
  const user = usernameInput.value.trim();
  const port = Number(portInput.value) || 22;
  const profileSelected = profileSelect.value !== "";
  const profileLabel = profileSelected
    ? (profileSelect.options[profileSelect.selectedIndex]?.textContent ?? "").trim()
    : "";

  if (!host && !user) {
    return { ready: false, summary: t("未配置连接") };
  }

  const endpoint = user && host ? `${user}@${host}:${port}` : host || user;
  const issues: string[] = [];

  if (!host) issues.push(t("缺主机"));
  if (!user) issues.push(t("缺用户名"));

  const needsPassword = authTypeSelect.value === "password";
  const missingPassword = needsPassword && !passwordInput.value;
  const missingKey = !needsPassword && !keyPathInput.value.trim();

  // 已保存的服务器密码在钥匙串里、连接时才读取，此时输入框为空不代表缺密码。
  if (missingPassword && !profileRemembersSecrets()) issues.push(t("缺密码"));
  if (missingKey) issues.push(t("缺私钥"));
  if (!profileSelected && (missingPassword || missingKey)) {
    issues.unshift(t("未选连接"));
  }

  if (issues.length === 0) {
    // 选择器本身已显示服务器名称，摘要只补充地址；未保存的连接才需要在摘要里说明。
    const summary = profileLabel ? endpoint : t("{endpoint} · 未保存", { endpoint });
    return { ready: true, summary };
  }

  return {
    ready: false,
    summary: endpoint,
    detail: issues.join(" · "),
  };
}


export function updateConnectionSummary() {
  const readiness = getConnectionReadiness();
  let summaryText: string;
  if (readiness.summary === t("未配置连接")) {
    summaryText = t("还没有配置服务器，点「编辑」添加");
  } else if (readiness.detail) {
    summaryText = `${readiness.summary} · ${readiness.detail}`;
  } else {
    summaryText = readiness.summary;
  }
  connectionSummary.textContent = summaryText;
  connectionSummary.title = summaryText;
  connectionSummary.classList.toggle("ok", readiness.ready);
  connectionSummary.classList.toggle("warn", !readiness.ready);
  updateAuthHintVisibility();
  highlightSelectedServer();
  updateBookmarkButton();
}


/** 「记住密码」只在需要输入秘密（密码登录或填了私钥口令）时显示，私钥免口令时不打扰用户。 */
export function updateAuthHintVisibility() {
  const needsSecret = authTypeSelect.value === "password" || Boolean(keyPassphraseInput.value);
  rememberRow.classList.toggle("hidden", !needsSecret);
  authHint.classList.toggle("hidden", !needsSecret);
  authHint.textContent = rememberSecretsInput.checked
    ? t("密码与私钥口令保存在 macOS 钥匙串中，不会写入配置文件")
    : t("不保存密码：只在本次运行中使用，下次选择该服务器时需要重新输入");
}


/**
 * 不记住密码的服务器，本次运行中输入过的密码/口令暂存在内存里：切到别的服务器再切回来不必重输，
 * 退出应用即丢失，正是「仅本次使用」的含义。
 */
export const sessionSecrets = new Map<string, { password: string; passphrase: string }>();


export function rememberForSession() {
  const id = profileSelect.value;
  if (!id || profileRemembersSecrets()) return;
  sessionSecrets.set(id, { password: passwordInput.value, passphrase: keyPassphraseInput.value });
}


/** 选中的已保存服务器是否把密码存在钥匙串里；未保存的连接视为否。 */
export function profileRemembersSecrets(): boolean {
  return currentProfile()?.remember_secrets ?? false;
}


export function validateConnectionBeforeTransfer(): void {
  const readiness = getConnectionReadiness();
  if (readiness.ready) return;

  const host = hostInput.value.trim();
  const user = usernameInput.value.trim();
  const profileSelected = profileSelect.value !== "";

  if (!host || !user) {
    throw new Error(
      t("还没有选择服务器。请在上方选择已保存的服务器，或到「服务器」页添加"),
    );
  }

  if (authTypeSelect.value === "password" && !passwordInput.value) {
    throw new Error(
      profileSelected
        ? t("所选服务器没有保存密码。请到「服务器」页填写密码并保存")
        : t("密码为空。请到「服务器」页填写密码"),
    );
  }

  if (authTypeSelect.value === "key" && !keyPathInput.value.trim()) {
    throw new Error(t("请到「服务器」页选择私钥文件"));
  }

  throw new Error(
    readiness.detail
      ? t("连接未就绪（{detail}）。请到「服务器」页检查", { detail: readiness.detail })
      : t("连接未就绪，请到「服务器」页检查"),
  );
}


export function updateAuthFields() {
  const isKey = authTypeSelect.value === "key";
  keyFields.classList.toggle("hidden", !isKey);
  passwordFields.classList.toggle("hidden", isKey);
  updateConnectionSummary();
  updateTransferStatePanel();
  updateAuthHintVisibility();
}


/** 本次运行中已确认可信的 host:port，避免每次浏览目录/预检都多一次握手（弱网下很贵）。 */
export const trustedHostsThisSession = new Set<string>();


/**
 * 任何会发送凭据的连接前调用：未知主机先展示指纹让用户确认，再写入应用 known_hosts。
 * 后端 connect_session 仍会强制校验，这里只是把「拒绝连接」变成可操作的确认流程。
 */
export async function ensureHostTrusted(): Promise<void> {
  const { host, port } = connectionPayload();
  const hostId = `${host}:${port}`;
  if (trustedHostsThisSession.has(hostId)) return;

  const info = await invoke<HostKeyInfo>("check_host_key", { request: { host, port } });
  if (info.status === "changed") {
    throw new Error(
      t("服务器 {hostId} 的指纹与已记录的不一致（当前 {fingerprint}），可能遭遇中间人攻击，已拒绝连接。\n", { hostId, fingerprint: info.fingerprint }) +
        t("如确认服务器重装过系统，请从 ~/.ssh/known_hosts 或 ~/Library/Application Support/sshutil/known_hosts 中删除旧记录后重试。"),
    );
  }
  if (info.status === "unknown") {
    const trusted = await showConfirm(
      t("首次连接该服务器"),
      `${hostId}\n${info.key_type}\n${info.fingerprint}\n\n` +
        t("请与服务器管理员核对上面的指纹（服务器上可运行 ssh-keygen -lf /etc/ssh/ssh_host_*_key.pub）。确认信任后将记住该服务器。"),
    );
    if (!trusted) throw new Error(t("未信任该服务器，已取消连接"));
    await invoke("trust_host_key", {
      request: { host, port, fingerprint: info.fingerprint },
    });
  }
  trustedHostsThisSession.add(hostId);
}


export function connectionPayload() {
  return {
    host: hostInput.value.trim(),
    port: Number(portInput.value) || 22,
    username: usernameInput.value.trim(),
    auth_type: authTypeSelect.value,
    password: passwordInput.value,
    key_path: keyPathInput.value.trim(),
    key_passphrase: keyPassphraseInput.value,
  };
}


export function setConnectionStatus(message: string, kind: "normal" | "error" | "ok" = "normal") {
  connectionStatus.textContent = message;
  connectionStatus.classList.remove("error", "ok");
  if (kind === "error") connectionStatus.classList.add("error");
  if (kind === "ok") connectionStatus.classList.add("ok");
}


export async function loadProfileSecrets(id: string): Promise<ProfileSecrets> {
  try {
    return await invoke<ProfileSecrets>("get_profile_secrets", { id });
  } catch {
    return { password: null, key_passphrase: null };
  }
}


/**
 * 连接前按需补齐所选服务器的密码/口令：启动时恢复断点不读钥匙串，
 * 拖到用户真正发起连接时才读，避免一打开应用就弹系统授权框。
 */
export async function ensureCredentialsLoaded() {
  const id = profileSelect.value;
  // 设置为不记住密码的服务器，钥匙串里本来就没有，读取只会白白触发授权弹窗。
  if (!id || !profileRemembersSecrets()) return;
  const needsPassword = authTypeSelect.value === "password" && !passwordInput.value;
  const needsPassphrase = authTypeSelect.value === "key" && !keyPassphraseInput.value;
  if (!needsPassword && !needsPassphrase) return;
  const secrets = await loadProfileSecrets(id);
  if (needsPassword && secrets.password) passwordInput.value = secrets.password;
  if (needsPassphrase && secrets.key_passphrase) keyPassphraseInput.value = secrets.key_passphrase;
  updateConnectionSummary();
}


/** 所有会发起 SSH 连接的入口统一走这里：补凭据 → 校验表单 → 确认主机指纹。 */
export async function prepareConnection() {
  await ensureCredentialsLoaded();
  validateConnectionBeforeTransfer();
  await ensureHostTrusted();
}

