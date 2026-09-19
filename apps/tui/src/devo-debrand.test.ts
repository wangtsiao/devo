/**
 * Devo TUI must not advertise upstream vendor brand in quit copy, update checks, or slash descriptions.
 */
import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { APP_NAME, APP_TITLE } from "../lib/coding-agent/src/config.js";
import { BUILTIN_SLASH_COMMANDS } from "../lib/coding-agent/src/core/slash-commands.js";
import {
  checkForNewPiVersion,
  getLatestPiRelease,
} from "../lib/coding-agent/src/utils/version-check.js";
import { DEVO_EXCLUDED_BUILTIN_COMMANDS } from "./host.js";
import { STRIP_VENDOR_COMMANDS } from "./native-agent-connection.js";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "../../..");

test("product name is devo so /quit is Quit devo", () => {
  assert.equal(APP_NAME, "devo");
  assert.equal(APP_TITLE, "devo");
  assert.deepEqual(
    BUILTIN_SLASH_COMMANDS.find((command) => command.name === "quit"),
    { name: "quit", description: "Quit devo" },
  );
});

test("slash command descriptions do not mention prime", () => {
  for (const command of BUILTIN_SLASH_COMMANDS) {
    assert.doesNotMatch(command.description, /prime/i, `/${command.name}: ${command.description}`);
  }
});

test("upstream update manifests are not consulted", async () => {
  assert.equal(await checkForNewPiVersion("0.0.0"), undefined);
  assert.equal(await getLatestPiRelease("0.0.0"), undefined);
});

test("host hides /update builtin and strips /changelog and /logs", () => {
  assert.deepEqual([...DEVO_EXCLUDED_BUILTIN_COMMANDS], ["traces", "update"]);
  assert.equal(STRIP_VENDOR_COMMANDS.has("/update"), true);
  assert.equal(STRIP_VENDOR_COMMANDS.has("/changelog"), true);
  assert.equal(STRIP_VENDOR_COMMANDS.has("/logs"), true);
});

test("coding-agent piConfig.name is devo", () => {
  const pkg = JSON.parse(
    readFileSync(join(repoRoot, "apps/tui/lib/coding-agent/package.json"), "utf8"),
  ) as { piConfig?: { name?: string; configDir?: string } };
  assert.deepEqual(pkg.piConfig, { name: "devo", configDir: ".devo" });
});
