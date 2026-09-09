/** Discovery sends IP and port separately; IPv6 must retain its brackets. */
export function discoveredAddress(host: string | undefined, port: number | undefined): string | null {
  if (!host?.trim() || !Number.isInteger(port) || port! <= 0 || port! > 65535) return null;
  const value = host.trim();
  return `${value.includes(':') && !value.startsWith('[') ? `[${value}]` : value}:${port}`;
}
