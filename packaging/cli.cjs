#!/usr/bin/env node
const { spawn } = require("node:child_process");
const { join } = require("node:path");

const child = spawn(join(__dirname, "boubou"), process.argv.slice(2), {
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
  console.error(`Could not start bobo: ${error.message}`);
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
