// Builds the bridge preload into dist/preload.js (single IIFE).
// Mirrors vite.config.ts's electrobun aliases — esbuild needs the same
// devkit mapping (ADR-002: the runtime injects this script's CONTENT).
import { build } from "esbuild";
import { readFileSync } from "node:fs";
import { resolve, relative, isAbsolute, sep } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const root = resolve(here, "..");
const devkitRoot = resolve(root, ".hutch/devkit");

function exactImportPattern(specifier) {
  return new RegExp(`^${specifier.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`);
}
function isWithin(rootDir, candidate) {
  const child = relative(rootDir, candidate);
  return child !== ".." && !child.startsWith(`..${sep}`) && !isAbsolute(child);
}

const aliasMap = {
  "electrobun/view": resolve(devkitRoot, "api/browser/index.ts"),
  
};

await build({
  entryPoints: [resolve(root, "src/preload/bridge.ts")],
  bundle: true,
  format: "iife",
  outfile: resolve(root, "dist/preload.js"),
  alias: aliasMap,
  logLevel: "info",
});
