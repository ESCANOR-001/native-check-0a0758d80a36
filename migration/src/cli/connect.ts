import { homedir } from "node:os";
import { join } from "node:path";

process.env.REMODEX_CONNECT_ONLY = "1";
process.env.OPENCODEX_HOME ||= join(homedir(), ".remodex-connect");

const argumentsWithoutProof = process.argv.slice(2).filter(argument => !argument.startsWith("--ocx-internal-launch-proof="));
const command = argumentsWithoutProof[0];
// Older installed services invoke this path directly with Bun. Preserve that
// registration on npm replacement and dispatch to the selected Rust generation.
// @ts-expect-error npm entry modules run directly without a declaration build.
const { tryNativeConnect } = await import('../../bin/native-connect.mjs');
if (await tryNativeConnect(join(import.meta.dir, '../..'), argumentsWithoutProof)) {
  process.exit(process.exitCode ?? 0);
}
const commands = new Set(["onboard", "start", "stop", "restart", "status", "ready", "service", "tray", "gui", "__tray-host", "__tray-start", "__tray-restart", "doctor", "version", "--version", "-v"]);

if (!command || command === "help" || command === "--help" || command === "-h") {
  console.log("Remodex Connect — native ChatGPT / Codex remote access\n\nCommands: onboard, start, stop, restart, status, ready, service, tray, gui, doctor, version\n\nExisting Codex settings are read-only. Provider setup, injection, and config migration are not supported.");
} else if (!commands.has(command)) {
  console.error(`'${command}' is not available in Remodex Connect. Existing Codex settings were not changed.`);
  process.exitCode = 64;
} else {
  await import("./index");
}
