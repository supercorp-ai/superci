#!/usr/bin/env node
// Starts SuperCI's own program for this computer. It comes in a package of
// its own for each system (an optional dependency, so npm installs only the
// one that fits).

import { spawn } from "node:child_process";
import { createRequire } from "node:module";

const packages = {
  "darwin arm64": "@superci/cli-darwin-arm64",
  "darwin x64": "@superci/cli-darwin-x64",
  "linux arm64": "@superci/cli-linux-arm64",
  "linux x64": "@superci/cli-linux-x64",
  "win32 x64": "@superci/cli-win32-x64",
  // Windows on arm64 runs the x64 program.
  "win32 arm64": "@superci/cli-win32-x64",
};

function program() {
  const name = packages[`${process.platform} ${process.arch}`];
  if (!name) {
    throw new Error(`no SuperCI program for ${process.platform} on ${process.arch} yet (there is one for macOS, Linux and Windows, on arm64 and x64)`);
  }
  try {
    return createRequire(import.meta.url).resolve(`${name}/superci${process.platform === "win32" ? ".exe" : ""}`);
  } catch {
    throw new Error(`${name} is not installed: install @superci/cli again without --omit=optional (or --no-optional)`);
  }
}

let path;
try {
  path = program();
} catch (error) {
  console.error(`superci: ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
}

const child = spawn(path, process.argv.slice(2), { stdio: "inherit" });

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    try {
      child.kill(signal);
    } catch {
      // It may already have ended.
    }
  });
}

child.on("error", (error) => {
  console.error(`superci: could not start (${error.message})`);
  process.exitCode = 1;
});
child.on("exit", (code, signal) => {
  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
  } else {
    process.exitCode = code ?? 1;
  }
});
