// 「传完后执行」：命令模板、常用命令收藏，以及远端命令输出的实时展示。
import { t } from "../i18n";
import { icons } from "../ui/icons";
import { invoke } from "@tauri-apps/api/core";
import { connectionPayload } from "./connection";
import { cancelBtn, commandConsole, commandConsoleCmd, commandMenu, commandOutput, hostInput, navTransferLive, portInput, postCommandInput, profileSelect, resultCommand, resultCommandLine, resultCommandOutput, saveCommandBtn } from "./dom";
import { describeError, fileNameOf, remoteDirOf } from "./format";
import { setBadge, setStatus } from "./progress";
import { currentProfile, saveProfileExtras } from "./servers";
import { state } from "./state";
import { CommandContext, CommandOutcome } from "./types";

/** 控制台只保留最近若干行，长时间输出的命令也不会让 DOM 无限增长。 */
export const CONSOLE_MAX_LINES = 200;

export const COMMAND_STORAGE_PREFIX = "resiliss-post-command:";

export const COMMAND_VARIABLES_HINT = "{remote_dir} {file_name} {remote_path} {files}";


/** 按服务器记住上次填写的命令：不同服务器的部署命令通常不同。 */
export function commandStorageKey(): string {
  return COMMAND_STORAGE_PREFIX + (profileSelect.value || `${hostInput.value.trim()}:${portInput.value}`);
}


export function restorePostCommand() {
  postCommandInput.value = localStorage.getItem(commandStorageKey()) ?? "";
  updateSaveCommandButton();
}


export function updateSaveCommandButton() {
  const profile = currentProfile();
  const command = postCommandInput.value.trim();
  const saved = Boolean(profile && command && profile.post_commands.includes(command));
  saveCommandBtn.disabled = !profile || !command || state.isTransferring;
  saveCommandBtn.classList.toggle("is-active", saved);
  saveCommandBtn.innerHTML = saved ? icons.starFilled : icons.star;
  saveCommandBtn.title = !profile
    ? t("保存服务器后才能保存常用命令")
    : saved
      ? t("从常用命令中移除")
      : t("保存为常用命令");
}


export async function toggleSavedCommand() {
  const profile = currentProfile();
  const command = postCommandInput.value.trim();
  if (!profile || !command) return;
  const commands = profile.post_commands.includes(command)
    ? profile.post_commands.filter((c) => c !== command)
    : [command, ...profile.post_commands];
  try {
    await saveProfileExtras(profile, { post_commands: commands });
    updateSaveCommandButton();
  } catch (error) {
    setStatus(describeError(error), "error");
  }
}


export function openCommandMenu() {
  const profile = currentProfile();
  const commands = profile?.post_commands ?? [];
  const footer = document.createElement("div");
  footer.append(t("可用变量（自动加引号）："));
  const vars = document.createElement("code");
  vars.textContent = COMMAND_VARIABLES_HINT;
  footer.append(vars);
  commandMenu.show(
    [
      {
        items: commands.map((command) => ({
          label: command,
          onSelect: () => {
            postCommandInput.value = command;
            postCommandInput.dispatchEvent(new Event("input"));
          },
          onRemove: () => {
            if (!profile) return;
            void saveProfileExtras(profile, {
              post_commands: commands.filter((c) => c !== command),
            }).then(() => {
              updateSaveCommandButton();
              openCommandMenu();
            });
          },
        })),
      },
    ],
    profile ? t("还没有常用命令\n填好命令后点输入框里的 ☆ 保存") : t("保存服务器后即可保存常用命令"),
    footer,
  );
}


export function commandContextFor(remotePaths: string[]): CommandContext {
  const last = remotePaths[remotePaths.length - 1] ?? "";
  return {
    remote_dir: remoteDirOf(last),
    file_name: fileNameOf(last),
    remote_path: last,
    files: remotePaths.map(fileNameOf),
  };
}


export function appendConsoleLine(line: string) {
  commandOutput.append(`${line}\n`);
  while (commandOutput.childNodes.length > CONSOLE_MAX_LINES) commandOutput.firstChild?.remove();
  commandOutput.scrollTop = commandOutput.scrollHeight;
}


/**
 * 传输全部成功后执行命令。期间留在进行中视图：地图保持完成态，下方控制台实时显示输出，
 * 「取消」按钮改为停止命令。返回 null 表示命令根本没跑起来（连接失败等），错误文本放在 error 里。
 */
export async function runPostCommand(
  template: string,
  context: CommandContext,
): Promise<{ outcome: CommandOutcome | null; error?: string }> {
  state.commandRunning = true;
  commandOutput.textContent = "";
  commandConsoleCmd.textContent = template;
  commandConsole.classList.remove("hidden");
  setBadge(t("执行命令中"), "verifying");
  cancelBtn.textContent = t("停止命令");
  cancelBtn.disabled = false;
  cancelBtn.classList.remove("hidden", "is-cancelling");
  navTransferLive.classList.remove("hidden");
  try {
    const outcome = await invoke<CommandOutcome>("run_remote_command", {
      request: { connection: connectionPayload(), template, context },
    });
    return { outcome };
  } catch (error) {
    return { outcome: null, error: describeError(error) };
  } finally {
    state.commandRunning = false;
    commandConsole.classList.add("hidden");
    cancelBtn.classList.add("hidden");
    navTransferLive.classList.add("hidden");
  }
}


/** 结果页的命令区：成功也展示末尾输出，方便确认服务确实起来了。 */
export function showCommandResult(outcome: CommandOutcome | null, error?: string) {
  if (!outcome && !error) {
    resultCommand.classList.add("hidden");
    return;
  }
  resultCommand.classList.remove("hidden");
  resultCommandLine.textContent = `$ ${outcome?.command ?? state.runPostCommandTemplate}`;
  const output = outcome ? outcome.output_tail.join("\n") : (error ?? "");
  resultCommandOutput.textContent = output || t("（没有输出）");
}


export function describeCommandFailure(outcome: CommandOutcome | null, error?: string): string {
  if (!outcome) return t("命令没能执行：{p1}", { p1: error ?? t("未知错误") });
  if (outcome.cancelled) return t("命令已被停止（远端进程可能仍在运行）");
  if (outcome.timed_out) return t("命令超过 10 分钟没有任何输出，已停止等待");
  return t("命令执行失败，退出码 {p1}", { p1: outcome.exit_code ?? t("未知") });
}

