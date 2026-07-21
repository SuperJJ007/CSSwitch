const RUNTIME_LIGHTS = new Set(["green", "amber", "red", "gray"]);

export const RUNTIME_STATUS_LABELS = Object.freeze({
  green: "运行正常",
  amber: "未运行 / 部分就绪",
  red: "需要处理",
  gray: "不适用",
  unknown: "状态未知",
});

export function normalizeRuntimeLight(value) {
  return RUNTIME_LIGHTS.has(value) ? value : "unknown";
}

export function aggregateRuntimeStatus(status, { mode = "proxy", officialState = "gray" } = {}) {
  if (mode === "official") return normalizeRuntimeLight(officialState);
  const values = [status?.proxy, status?.sandbox, status?.upstream].map(normalizeRuntimeLight);
  if (values.includes("red")) return "red";
  if (values.includes("unknown")) return "gray";
  const applicable = values.filter((value) => value !== "gray");
  if (!applicable.length) return "gray";
  if (applicable.every((value) => value === "green")) return "green";
  return "amber";
}

export function scienceRuntimeStatusText(science) {
  const runtime = science && typeof science === "object" ? science.runtime : null;
  if (!runtime || typeof runtime !== "object") {
    return "更新由官方 Claude Science 管理；隔离实例不登录或检查更新。";
  }
  const version = typeof runtime.version === "string" && runtime.version.trim()
    ? runtime.version.trim()
    : "版本未知";
  switch (runtime.source) {
    case "official_downloaded":
      return `官方已下载 Runtime · ${version}；隔离实例仅复用程序文件。`;
    case "installed_app":
      return `App 内置备用 Runtime · ${version}；官方下载 Runtime 当前不可用。`;
    case "explicit":
      return `开发 Override Runtime · ${version}；更新不由隔离实例管理。`;
    case "cached_once":
      return `一次性缓存 Runtime · ${version}；不会保存为默认选择。`;
    default:
      return "Runtime 来源无法确认；更新仍由官方 Claude Science 管理。";
  }
}
