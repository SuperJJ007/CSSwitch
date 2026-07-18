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
