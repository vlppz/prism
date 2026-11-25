#!/usr/bin/env node
// macOS: builds `prism-helper` (the root network service) and puts it where
// Tauri expects external binaries: src-tauri/binaries/prism-helper-<triple>.
// Windows: checks that wintun.dll is in src-tauri/resources (the tunnel runs
// inside the app there, so no helper is needed).
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, chmodSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const release = process.argv.includes("--release");
const triple =
  process.env.TAURI_ENV_TARGET_TRIPLE ??
  execFileSync("rustc", ["-vV"], { encoding: "utf8" }).match(/^host: (.+)$/m)[1];

if (triple.includes("windows")) {
  const dll = join(root, "src-tauri", "resources", "wintun.dll");
  if (!existsSync(dll)) {
    console.error(`✗ missing ${dll}\n  run: scripts/fetch-wintun.sh`);
    process.exit(1);
  }
  console.log("✓ wintun.dll present");
  process.exit(0);
}

const args = ["build", "-p", "prism-helper", "--target", triple];
if (release) args.push("--release");
execFileSync("cargo", args, { cwd: root, stdio: "inherit" });
const built = join(root, "target", triple, release ? "release" : "debug", "prism-helper");
mkdirSync(join(root, "src-tauri", "binaries"), { recursive: true });
const dest = join(root, "src-tauri", "binaries", `prism-helper-${triple}`);
copyFileSync(built, dest);
chmodSync(dest, 0o755);
console.log(`✓ prism-helper → ${dest}`);
