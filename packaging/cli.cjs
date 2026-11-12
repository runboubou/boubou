#!/usr/bin/env node
const { spawn } = require("node:child_process");
const { join, sep } = require("node:path");

// The package carries the Apple Silicon build at its root and the x86-64 Linux build in linux-x64/.
const binary = {
  "darwin-arm64": join(__dirname, "boubou"),
  "linux-x64": join(__dirname, "linux-x64", "boubou"),
}[`${process.platform}-${process.arch}`];
if (!binary) {
  console.error("Boubou's npm package runs on Apple Silicon Macs and x86-64 Linux.");
  process.exit(1);
}
// Where npm put this copy decides what its user types. npx keeps what it fetches in its cache's
// _npx folder, where no `boubou` command reaches it; `npm install --global` uses lib/node_modules.
// Told which, Boubou names commands that work and says how to get a newer version.
const folders = __dirname.split(sep);
const npm = folders.includes("_npx") ? "npx" : folders.at(-3) === "lib" ? "global" : undefined;
const child = spawn(binary, process.argv.slice(2), {
  stdio: "inherit",
  env: { ...process.env, ...(npm && { BOUBOU_NPM: npm }) },
});
let shutdownSignal;
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    shutdownSignal ??= signal;
    // Rust handles Ctrl+C by restoring the terminal and reaping its children.
    child.kill("SIGINT");
  });
}
child.once("error", (error) => {
  console.error(`Could not start boubou: ${error.message}`);
  process.exitCode = 1;
});
child.once("exit", (code, signal) => {
  signal = shutdownSignal ?? signal;
  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
  } else {
    process.exitCode = code ?? 1;
  }
});
