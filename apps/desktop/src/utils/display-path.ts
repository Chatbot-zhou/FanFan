/// 去掉 Windows 长路径前缀 `\\?\`（含 `\\?\UNC\`），保留其余路径内容。
/// 这些前缀来自 Windows 长路径接口，直接展示会变成“两条斜杠一个问号”。
export function stripWindowsLongPathPrefix(path: string): string {
  if (path.startsWith("\\\\?\\unc\\")) return path.slice("\\\\?\\unc\\".length);
  if (path.startsWith("\\\\?\\")) return path.slice("\\\\?\\".length);
  return path;
}

/// 只清理前缀、保留完整路径，用于需要展示完整目录/文件路径的场合。
export function cleanFullPath(path: string): string {
  return stripWindowsLongPathPrefix(path);
}

export function displayPath(path: string): string {
  const normalized = stripWindowsLongPathPrefix(path).replaceAll("/", "\\");
  const absolute = /^[a-zA-Z]:\\/.test(normalized) || normalized.startsWith("\\\\");
  if (!absolute) return normalized;
  const parts = normalized.split("\\").filter(Boolean);
  const visible = parts.slice(-3).join("\\");
  return parts.length > 3 ? `…\\${visible}` : visible;
}
