// 服务器配置：已保存服务器、~/.ssh/config 导入、表单与配置之间的同步。
import { t } from "../i18n";
import { invoke } from "@tauri-apps/api/core";
import { restorePostCommand } from "./command";
import { loadProfileSecrets, sessionSecrets, setConnectionStatus, updateAuthFields, updateAuthHintVisibility, updateConnectionSummary } from "./connection";
import { authTypeSelect, hostInput, hostSelect, jumpProfileSelect, keyPassphraseInput, keyPathInput, navServerCount, passwordInput, portInput, profileNameInput, profileSelect, rememberSecretsInput, serverList, usernameInput } from "./dom";
import { state } from "./state";
import { ConnectionProfile, SshHostEntry } from "./types";

/** 最近使用的目录最多列几个：太多会把收藏挤出视野。 */
export const RECENT_DIR_LIMIT = 6;


export function currentProfile(): ConnectionProfile | undefined {
  return state.savedProfiles.find((p) => p.id === profileSelect.value);
}


export async function saveProfileExtras(
  profile: ConnectionProfile,
  changes: { bookmarks?: string[]; post_commands?: string[] },
) {
  const updated = await invoke<ConnectionProfile>("update_connection_profile_extras", {
    request: { id: profile.id, ...changes },
  });
  const index = state.savedProfiles.findIndex((p) => p.id === updated.id);
  if (index >= 0) state.savedProfiles[index] = updated;
  return updated;
}


export function restoreCredentialsFromProfile(host: string, port: number, username: string) {
  const profile = state.savedProfiles.find(
    (p) => p.host === host && p.port === port && p.username === username,
  );
  if (!profile) return;

  profileSelect.value = profile.id;
  profileNameInput.value = profile.name;
  setJumpSelection(profile.jump_profile_id);

  // 这里由启动时自动恢复断点触发，不主动读钥匙串（会弹授权框）；
  // 真正连接前由 ensureCredentialsLoaded 按需补齐密码。

  restorePostCommand();
  setConnectionStatus(t("已匹配服务器「{name}」", { name: profile.name }), "ok");
  updateConnectionSummary();
}


/** 按 host/port/user 找到已保存的服务器，用于带出凭据；找不到时只回填地址。 */
export function findProfileFor(host: string, port: number, username: string): ConnectionProfile | undefined {
  return state.savedProfiles.find((p) => p.host === host && p.port === port && p.username === username);
}


export function defaultProfileName(): string {
  const alias = hostSelect.value.trim();
  if (alias) return alias;
  const user = usernameInput.value.trim();
  const host = hostInput.value.trim();
  if (user && host) return `${user}@${host}`;
  return host || user;
}


export async function applyProfile(profile: ConnectionProfile) {
  hostInput.value = profile.host;
  portInput.value = String(profile.port);
  usernameInput.value = profile.username;
  authTypeSelect.value = profile.auth_kind === "private_key" ? "key" : "password";
  if (profile.key_path) {
    keyPathInput.value = profile.key_path;
  } else if (profile.auth_kind === "private_key") {
    keyPathInput.value = "";
  }

  passwordInput.value = "";
  keyPassphraseInput.value = "";
  profileNameInput.value = profile.name;
  setJumpSelection(profile.jump_profile_id);
  updateAuthFields();
  updateConnectionSummary();

  restorePostCommand();
  rememberSecretsInput.checked = profile.remember_secrets;
  updateAuthHintVisibility();
  if (!profile.remember_secrets) {
    const cached = sessionSecrets.get(profile.id);
    if (cached) {
      passwordInput.value = cached.password;
      keyPassphraseInput.value = cached.passphrase;
      updateConnectionSummary();
      setConnectionStatus(t("已加载「{name}」", { name: profile.name }), "ok");
      return;
    }
    setConnectionStatus(
      t("「{name}」设置为不保存密码，请输入密码后连接", { name: profile.name }),
      "normal",
    );
    return;
  }
  // 用户主动选择服务器时才读钥匙串：此时弹出系统授权框是可预期的。
  const secrets = await loadProfileSecrets(profile.id);
  if (profileSelect.value !== profile.id) return; // 读取期间用户已切到别的服务器
  passwordInput.value = secrets.password ?? "";
  keyPassphraseInput.value = secrets.key_passphrase ?? "";
  updateConnectionSummary();
  updateAuthHintVisibility();

  if (profile.auth_kind === "password") {
    setConnectionStatus(
      secrets.password
        ? t("已加载「{name}」，密码已自动填入", { name: profile.name })
        : t("已加载「{name}」，请填写密码后点保存", { name: profile.name }),
      secrets.password ? "ok" : "normal",
    );
  } else {
    setConnectionStatus(t("已加载「{name}」", { name: profile.name }), "ok");
  }
}


export async function loadProfiles() {
  state.savedProfiles = await invoke<ConnectionProfile[]>("list_saved_profiles");
  profileSelect.replaceChildren();
  const defaultOption = document.createElement("option");
  defaultOption.value = "";
  defaultOption.textContent = t("未保存的服务器");
  profileSelect.appendChild(defaultOption);

  for (const profile of state.savedProfiles) {
    const option = document.createElement("option");
    option.value = profile.id;
    option.textContent = profile.name;
    profileSelect.appendChild(option);
  }
  renderJumpOptions();
  renderServerList();
}


export function profileById(id: string | null | undefined): ConnectionProfile | undefined {
  return id ? state.savedProfiles.find((p) => p.id === id) : undefined;
}


/**
 * 「经由」下拉框：列出可作为跳板机的其他已保存服务器。不能选自己，
 * 也不列出最终又经由当前服务器的那些（会成环）；后端保存时还会再校验一次。
 */
export function renderJumpOptions() {
  const selfId = profileSelect.value;
  const keep = jumpProfileSelect.value;
  const direct = document.createElement("option");
  direct.value = "";
  direct.textContent = t("直接连接");
  const candidates = state.savedProfiles.filter((p) => p.id !== selfId && !jumpChainIncludes(p, selfId));
  jumpProfileSelect.replaceChildren(
    direct,
    ...candidates.map((p) => {
      const option = document.createElement("option");
      option.value = p.id;
      option.textContent = `${p.name} (${p.username}@${p.host}${p.port === 22 ? "" : `:${p.port}`})`;
      return option;
    }),
  );
  jumpProfileSelect.value = candidates.some((p) => p.id === keep) ? keep : "";
}


function jumpChainIncludes(profile: ConnectionProfile, id: string): boolean {
  if (!id) return false;
  const seen = new Set<string>();
  let hop = profileById(profile.jump_profile_id);
  while (hop && !seen.has(hop.id)) {
    if (hop.id === id) return true;
    seen.add(hop.id);
    hop = profileById(hop.jump_profile_id);
  }
  return false;
}


export function setJumpSelection(jumpId: string | null | undefined) {
  renderJumpOptions();
  jumpProfileSelect.value = profileById(jumpId) ? (jumpId as string) : "";
}


/**
 * ssh config 里的 ProxyJump 写的是别名或 user@host:port，这里找出对应的已保存服务器：
 * 先按名称（导入时名称默认就是别名），再按地址。找不到返回 undefined，由调用方提示先保存跳板机。
 */
export function findProfileForJumpSpec(spec: string): ConnectionProfile | undefined {
  const byName = state.savedProfiles.find((p) => p.name === spec);
  if (byName) return byName;
  const match = /^(?:([^@]+)@)?(\[[^\]]+\]|[^:]+)(?::(\d+))?$/.exec(spec);
  if (!match) return undefined;
  const [, user, rawHost, rawPort] = match;
  const host = rawHost.replace(/^\[|\]$/g, "");
  const port = rawPort ? Number(rawPort) : 22;
  return state.savedProfiles.find(
    (p) => p.host === host && p.port === port && (!user || p.username === user),
  );
}


/** 服务器页左侧列表；选中项即「当前使用的服务器」，与传输页顶部的选择器保持一致。 */
export function renderServerList() {
  navServerCount.textContent = String(state.savedProfiles.length);
  navServerCount.classList.toggle("hidden", state.savedProfiles.length === 0);
  if (state.savedProfiles.length === 0) {
    const empty = document.createElement("div");
    empty.className = "list-empty";
    empty.textContent = t("还没有保存的服务器\n在右侧填写后点「保存」");
    serverList.replaceChildren(empty);
    return;
  }
  serverList.replaceChildren(
    ...state.savedProfiles.map((profile) => {
      const item = document.createElement("button");
      item.type = "button";
      item.className = "server-item";
      item.dataset.id = profile.id;
      item.setAttribute("role", "option");
      const name = document.createElement("span");
      name.className = "server-item-name";
      name.textContent = profile.name;
      const meta = document.createElement("span");
      meta.className = "server-item-meta";
      const jump = profileById(profile.jump_profile_id);
      meta.textContent =
        `${profile.username}@${profile.host}${profile.port === 22 ? "" : `:${profile.port}`}` +
        (jump ? t(" · 经 {name}", { name: jump.name }) : "");
      item.append(name, meta);
      item.disabled = state.isTransferring;
      item.addEventListener("click", () => {
        profileSelect.value = profile.id;
        void applyProfile(profile);
      });
      return item;
    }),
  );
  highlightSelectedServer();
}


export function highlightSelectedServer() {
  serverList.querySelectorAll<HTMLButtonElement>(".server-item").forEach((item) => {
    const selected = item.dataset.id === profileSelect.value;
    item.classList.toggle("selected", selected);
    item.setAttribute("aria-selected", String(selected));
  });
}


/** 新建：清空编辑表单并取消选中，避免用户在已有服务器上改着改着覆盖了它。 */
export function startNewServer() {
  profileSelect.value = "";
  profileNameInput.value = "";
  hostSelect.value = "";
  hostInput.value = "";
  portInput.value = "22";
  usernameInput.value = "";
  passwordInput.value = "";
  keyPassphraseInput.value = "";
  rememberSecretsInput.checked = true;
  setJumpSelection(null);
  setConnectionStatus("");
  updateAuthFields();
  updateConnectionSummary();
  profileNameInput.focus();
}


export async function loadHosts() {
  const hosts = await invoke<SshHostEntry[]>("list_hosts");
  for (const host of hosts) {
    const option = document.createElement("option");
    option.value = host.alias;
    option.textContent = host.alias;
    hostSelect.appendChild(option);
  }
}


export async function applyHostSelection(alias: string) {
  if (!alias) return;
  const entry = await invoke<SshHostEntry | null>("get_host", { alias });
  if (!entry) return;

  if (entry.hostname) hostInput.value = entry.hostname;
  if (entry.user) usernameInput.value = entry.user;
  if (entry.port) portInput.value = String(entry.port);
  if (entry.identity_file) {
    authTypeSelect.value = "key";
    keyPathInput.value = entry.identity_file;
    updateAuthFields();
  }
  profileNameInput.value = alias;
  applyProxyJump(entry.proxy_jump);
  updateConnectionSummary();
}


function applyProxyJump(spec: string | null) {
  if (!spec) {
    setJumpSelection(null);
    return;
  }
  const jump = findProfileForJumpSpec(spec);
  setJumpSelection(jump?.id);
  if (!jump) {
    setConnectionStatus(
      t("ssh 配置中该主机经由 {spec} 连接，请先导入并保存 {spec}，再在「经由」中选择它", { spec }),
      "error",
    );
  }
}

