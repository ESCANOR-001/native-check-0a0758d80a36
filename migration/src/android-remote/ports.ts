// Explicit alternate ports let a compatibility/upgrade rehearsal stay isolated
// from the user's existing bridge. Normal installations keep their pinned ports.
function configuredPort(name: string, fallback: number): number {
  const value = process.env[name];
  if (value === undefined) return fallback;
  if (!/^[1-9][0-9]{0,4}$/.test(value) || Number(value) > 65535) {
    throw new Error(`Invalid ${name}; refusing to use another bridge's port`);
  }
  return Number(value);
}
export const DEFAULT_ANDROID_GATEWAY_PORT = configuredPort('REMODEX_ANDROID_GATEWAY_PORT', 10105);
export const DEFAULT_CODEX_APP_SERVER_PORT = configuredPort('REMODEX_CODEX_APP_SERVER_PORT', 10106);
export const ANDROID_TUNNEL_ORIGIN = `http://127.0.0.1:${DEFAULT_ANDROID_GATEWAY_PORT}`;
