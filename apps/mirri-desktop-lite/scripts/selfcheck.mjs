// Startup self-check (R9): launches the lite shell in dev mode and asserts
// the boot log answers "where did the daemon come from" and "did the bridge
// preload inject". Exit 0 = healthy, 1 = assertion failure/timeout.
import { spawn } from "node:child_process";
import { existsSync, rmSync } from "node:fs";

const APP = new URL("..", import.meta.url).pathname;
const LOG = "/tmp/lite-selfcheck.log";
const TIMEOUT_MS = 45_000;

const asserts = [
  /reusing (?:live daemon|daemon from state|daemon that won)|spawning daemon/,
  /\[lite-preload\] window\.mirriDesktop injected/,
];

if (existsSync(LOG)) rmSync(LOG);
const child = spawn("hutch", ["electrobun", "dev"], {
  cwd: APP,
  env: { ...process.env, ELECTROBUN_CONSOLE: "1" },
  stdio: ["ignore", "pipe", "pipe"],
  detached: false,
});

let out = "";
child.stdout.on("data", (d) => (out += d));
child.stderr.on("data", (d) => (out += d));

const timer = setTimeout(() => finish(false, "timeout waiting for boot markers"), TIMEOUT_MS);

let done = false;
function finish(ok, reason) {
  if (done) return;
  done = true;
  clearTimeout(timer);
  try {
    if (process.platform === "darwin") {
      spawn("pkill", ["-f", "Mirri Code Lite-dev"]);
      spawn("pkill", ["-f", "hutch electrobun dev"]);
    }
  } catch {}
  console.log("--- boot log ---");
  console.log(out.split("\n").filter((l) => /\[lite|lite-preload|hutch|error/i.test(l)).join("\n"));
  if (!ok) {
    console.error(`SELF-CHECK FAILED: ${reason}`);
    process.exit(1);
  }
  console.log("SELF-CHECK PASSED");
  process.exit(0);
}

const poll = setInterval(() => {
  const missing = asserts.filter((re) => !re.test(out));
  if (missing.length === 0) {
    clearInterval(poll);
    finish(true);
  }
}, 500);
