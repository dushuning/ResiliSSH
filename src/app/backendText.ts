// 后端错误文案的本地化补充：把常见的原始错误改写成用户能看懂、知道下一步怎么做的提示。
import { t } from "../i18n";
import { authTypeSelect, passwordInput, remotePathInput } from "./dom";

/**
 * 后端错误文本中的固定片段（中英文各一份）。后端按界面语言出文案，但切换语言前产生的消息
 * 仍是旧语言，所以两种都要认。与 src-tauri 中对应文案保持一致，修改时两边同步。
 */
export const BACKEND_TEXT = {
  uploadRunning: ["已有上传任务在进行中", "An upload is already in progress"],
  downloadRunning: ["已有下载任务在进行中", "A download is already in progress"],
  remotePath: ["远端路径错误", "远端路径「", "Remote path error", 'Remote path "'],
  passwordMissing: ["请填写密码", "Please enter a password"],
  authFailed: ["认证失败", "服务器拒绝了提供的凭据", "Authentication failed", "The server rejected the credentials"],
  sftpError: ["SFTP 错误", "SFTP error"],
} as const;


export function matchesBackend(message: string, fragments: readonly string[]): boolean {
  return fragments.some((fragment) => message.includes(fragment));
}


export function clarifyTransferError(message: string): string {
  if (matchesBackend(message, BACKEND_TEXT.remotePath)) {
    return message;
  }
  if (matchesBackend(message, BACKEND_TEXT.passwordMissing)) {
    return t("未填写密码。请到「服务器」页填写密码");
  }
  if (
    matchesBackend(message, BACKEND_TEXT.authFailed) &&
    authTypeSelect.value === "password" &&
    !passwordInput.value
  ) {
    return t("未填写密码。请到「服务器」页填写密码");
  }
  if (matchesBackend(message, BACKEND_TEXT.authFailed)) {
    return t("SSH 认证失败，密码或私钥可能不正确。\n{message}", { message });
  }
  const remote = remotePathInput.value.trim();
  if (/no such file|sftp\(2\)/i.test(message) && remote) {
    return t("远端路径错误：{remote}\n该路径或上级目录在服务器上不存在（本地文件不受影响）", { remote });
  }
  if (matchesBackend(message, BACKEND_TEXT.sftpError) && remote && !message.includes(remote)) {
    return t("远端路径错误：{remote}\n{message}", { remote, message });
  }
  return message;
}

