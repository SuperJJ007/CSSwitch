export const CODEX_AUTH_REASONS = new Set([
  "ready",
  "state_missing",
  "state_uncommitted",
  "oauth_missing",
  "thinking_missing",
  "record_mismatch",
]);

const ERROR_CODES = new Set([
  "codex_login_required",
  "codex_auth_unavailable",
  "codex_auth_busy",
]);

const ERROR_CAUSES = new Set([
  "keychain_unavailable",
  "interaction_timeout",
  "sidecar_spawn_failed",
  "sidecar_protocol_error",
  "identity_mismatch",
  "auth_state_invalid",
  "storage_unavailable",
  "unsupported_platform",
  "auth_state_changed",
]);

const RETRYABLE_CAUSES = new Set([
  "keychain_unavailable",
  "interaction_timeout",
  "storage_unavailable",
  "auth_state_changed",
]);

export function parseCodexAuthCommandError(value) {
  if (!value || typeof value !== "object" || Array.isArray(value) || !("code" in value)) return null;
  const keys = Object.keys(value);
  if (keys.some((key) => !["code", "reason", "cause", "retryable"].includes(key)) ||
      !ERROR_CODES.has(value.code) || typeof value.retryable !== "boolean") {
    throw new Error("CSSwitch Codex 认证错误协议不匹配。");
  }
  const hasReason = Object.prototype.hasOwnProperty.call(value, "reason");
  const hasCause = Object.prototype.hasOwnProperty.call(value, "cause");
  if (value.code === "codex_login_required") {
    if (!hasReason || !CODEX_AUTH_REASONS.has(value.reason) || value.reason === "ready" || hasCause || value.retryable) {
      throw new Error("CSSwitch Codex 登录错误组合不匹配。");
    }
  } else if (value.code === "codex_auth_unavailable") {
    if (hasReason || !hasCause || !ERROR_CAUSES.has(value.cause) ||
        value.retryable !== RETRYABLE_CAUSES.has(value.cause)) {
      throw new Error("CSSwitch Codex 不可用错误组合不匹配。");
    }
  } else if (hasReason || hasCause || value.retryable !== true) {
    throw new Error("CSSwitch Codex 忙碌错误组合不匹配。");
  }
  return {
    code: value.code,
    reason: value.reason || null,
    cause: value.cause || null,
    retryable: value.retryable,
  };
}

export function formatCodexAuthCommandError(error) {
  if (error.code === "codex_auth_busy") {
    return "另一项 Codex 登录、状态检查或启动操作正在进行，请稍后重试。";
  }
  if (error.code === "codex_login_required") {
    if (["state_missing", "state_uncommitted"].includes(error.reason)) {
      return "CSSwitch Codex 尚未登录；请在“设置 > Codex 账号与连接”中登录。原生 Codex 登录不会被复用或修改。";
    }
    const labels = {
      oauth_missing: "OAuth 记录缺失",
      thinking_missing: "thinking 记录缺失",
      record_mismatch: "记录 generation/epoch 不匹配",
    };
    return "CSSwitch Codex 本地认证记录不完整（" + labels[error.reason] + "）；请先检查状态，不会自动修复或重新登录。";
  }
  const causes = {
    keychain_unavailable: "旧版 CSSwitch 本地认证存储不可用",
    interaction_timeout: "本地认证检查超时",
    sidecar_spawn_failed: "认证 sidecar 无法启动",
    sidecar_protocol_error: "认证 sidecar 协议不匹配",
    identity_mismatch: "安装包内 Gateway 与 Desktop 不匹配",
    auth_state_invalid: "本地认证状态损坏或无法解析",
    storage_unavailable: "认证存储暂不可用",
    unsupported_platform: "当前平台不支持该认证存储",
    auth_state_changed: "认证状态在检查期间发生变化",
  };
  return "CSSwitch Codex 认证状态不可用（" + causes[error.cause] + "）。" + (error.retryable ? "可以重试。" : "请先修复此问题再重试。");
}
