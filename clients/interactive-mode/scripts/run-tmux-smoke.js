/**
 * Cross-platform entry for `npm run test:tmux`.
 * Skips with exit 0 when tmux is unavailable (expected on windows-latest CI).
 */
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const script = path.join(root, "scripts", "tmux-tui-smoke.sh");
const scenario = process.argv[2];

function hasCmd(cmd) {
  const probe =
    process.platform === "win32"
      ? spawnSync("where.exe", [cmd], { encoding: "utf8" })
      : spawnSync("sh", ["-c", `command -v ${cmd}`], { encoding: "utf8" });
  return probe.status === 0;
}

if (!hasCmd("tmux")) {
  console.log("skip: tmux not available (expected on windows-latest CI)");
  process.exit(0);
}

if (!hasCmd("bash")) {
  console.log("skip: bash not available to run tmux-tui-smoke.sh");
  process.exit(0);
}

const args = [script];
if (scenario) args.push(scenario);

const result = spawnSync("bash", args, {
  cwd: root,
  stdio: "inherit",
  env: process.env,
});

if (result.error) {
  console.log(`skip: failed to spawn bash (${result.error.message})`);
  process.exit(0);
}

process.exit(result.status ?? 1);
