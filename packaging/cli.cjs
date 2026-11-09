#!/usr/bin/env node
const { spawn } = require("node:child_process");
const { join } = require("node:path");

// The package carries the Apple Silicon build at its root and the x86-64 Linux build in linux-x64/.
const binary = {
  "darwin-arm64": join(__dirname, "boubou"),
  "linux-x64": join(__dirname, "linux-x64", "boubou"),
}[`${process.platform}-${process.arch}`];
if (!binary) {
  console.error("Boubou's npm package runs on Apple Silicon Macs and x86-64 Linux.");
  process.exit(1);
}
const child = spawn(binary, process.argv.slice(2), {
  stdio: "inherit",
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
