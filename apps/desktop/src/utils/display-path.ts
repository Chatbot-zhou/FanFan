export function displayPath(path: string): string {
  const normalized = path.replaceAll("/", "\\");
  const absolute = /^[a-zA-Z]:\\/.test(normalized) || normalized.startsWith("\\\\");
  if (!absolute) return normalized;
  const parts = normalized.split("\\").filter(Boolean);
  const visible = parts.slice(-3).join("\\");
  return parts.length > 3 ? `…\\${visible}` : visible;
}
