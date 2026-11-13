#!/usr/bin/env node
const { spawn, execFile } = require("node:child_process");
const fs = require("node:fs/promises");
const { constants } = require("node:fs");
const { join, sep, resolve } = require("node:path");
const { homedir } = require("node:os");

const platform = `${process.platform}-${process.arch}`;
const executable = { "darwin-arm64": "boubou", "linux-x64": "linux-x64/boubou" }[platform];
const args = process.argv.slice(2);
let activeChild;
let shutdownSignal;
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    shutdownSignal ??= signal;
    activeChild?.kill("SIGINT");
  });
}

function finish(code, signal) {
  signal = shutdownSignal ?? signal;
  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
  } else {
    process.exitCode = code ?? 1;
  }
}

// Validate before using registry text as a path, npm argument or terminal message.
function version(text) {
  if (typeof text !== "string" || !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/.test(text)) return;
  const [release, pre] = text.split(/-(.*)/s);
  const parts = pre?.split(".");
  if (parts?.some((part) => /^0\d+$/.test(part))) return;
  return { numbers: release.split(".").map(BigInt), pre: parts };
}

function compare(a, b) {
  const left = version(a), right = version(b);
  for (let i = 0; i < 3; i++) {
    if (left.numbers[i] !== right.numbers[i]) return left.numbers[i] > right.numbers[i] ? 1 : -1;
  }
  if (!left.pre || !right.pre) return Number(!left.pre) - Number(!right.pre);
  for (let i = 0; i < Math.max(left.pre.length, right.pre.length); i++) {
    const x = left.pre[i], y = right.pre[i];
    if (x === y) continue;
    if (x === undefined || y === undefined) return x === undefined ? -1 : 1;
    const xn = /^\d+$/.test(x), yn = /^\d+$/.test(y);
    if (xn !== yn) return xn ? -1 : 1;
    return (xn ? BigInt(x) > BigInt(y) : x > y) ? 1 : -1;
  }
  return 0;
}

function channel(text) { return version(text).pre?.[0] ?? "latest"; }
function eligible(candidate, running) {
  return version(candidate) && (!version(candidate).pre || channel(candidate) === channel(running));
}

function cacheDirectory() {
  let cache = process.env.BOUBOU_HOME;
  for (let i = 0; i < args.length; i++) {
    if (args[i] === "--cache-dir") cache = args[++i];
    else if (args[i].startsWith("--cache-dir=")) cache = args[i].slice(12);
  }
  return resolve(cache || (process.platform === "darwin"
    ? join(homedir(), "Library", "Caches", "boubou")
    : join(process.env.XDG_CACHE_HOME || join(homedir(), ".cache"), "boubou")));
}

async function ready(directory, expected) {
  try {
    const manifest = JSON.parse(await fs.readFile(join(directory, "package.json"), "utf8"));
    if (manifest.name !== "boubou" || manifest.version !== expected || !version(expected)) return false;
    await fs.access(join(directory, "cli.cjs"), constants.R_OK);
    await fs.access(join(directory, executable), constants.X_OK);
    return true;
  } catch { return false; }
}

function npm(command, timeout) {
  return new Promise((accept, reject) => {
    if (shutdownSignal) return reject(new Error("Update interrupted"));
    activeChild = execFile("npm", command, { cwd: homedir(), timeout, killSignal: "SIGKILL", maxBuffer: 1024 * 1024 }, (error, stdout) => {
      activeChild = undefined;
      // npm errors can include private registry URLs or credentials. Keep diagnostics local to npm.
      if (error) reject(new Error(error.code === "ENOENT" ? "npm is unavailable" : "npm request failed or timed out"));
      else if (shutdownSignal) reject(new Error("Update interrupted"));
      else accept(stdout);
    });
  });
}

// Asks in the terminal through a short-lived shell, so this process never reads the input the
// command needs. Without a terminal, such as in scripts, the update goes ahead without asking.
function agree(question) {
  if (!process.stdin.isTTY || !process.stderr.isTTY) return Promise.resolve(true);
  return new Promise((accept) => {
    let answer = "";
    activeChild = spawn("/bin/sh", ["-c", 'printf "%s" "$1" >&2; read -r answer && printf "%s" "$answer"', "sh", question],
      { stdio: ["inherit", "pipe", "inherit"] });
    activeChild.stdout.on("data", (chunk) => { answer += chunk; });
    activeChild.once("error", () => accept(false));
    activeChild.once("close", (code) => {
      activeChild = undefined;
      accept(code === 0 && !shutdownSignal && !/^n/i.test(answer.trim()));
    });
  });
}

async function selectPackage() {
  const own = JSON.parse(await fs.readFile(join(__dirname, "package.json"), "utf8")).version;
  let selected = { version: own, directory: __dirname };
  const versions = join(cacheDirectory(), "npm", "versions");
  // Only atomically published version directories are candidates, never an interrupted staging dir.
  for (const name of await fs.readdir(versions).catch(() => [])) {
    const cachedVersion = name.split("_")[0];
    const directory = join(versions, name, "node_modules", "boubou");
    if (eligible(cachedVersion, own) && compare(cachedVersion, selected.version) > 0 && await ready(directory, cachedVersion)) {
      selected = { version: cachedVersion, directory };
    }
  }
  // A previously updated launcher owns all future checks, including fixes to this updater.
  if (selected.directory !== __dirname) return { ...selected, checked: false };
  const commandArgs = args.filter((arg, i) => arg !== "--cache-dir" && args[i - 1] !== "--cache-dir" && !arg.startsWith("--cache-dir="));
  if (process.env.BOUBOU_NO_UPDATE_CHECK || process.env.npm_config_offline === "true" || args.includes("--offline") || commandArgs[0] === "doctor") return selected;
  // Ask npm at most once an hour, whatever its answer, so most starts never wait for the network.
  const checked = join(versions, "..", "checked");
  if (Date.now() - (await fs.stat(checked).catch(() => ({ mtimeMs: 0 }))).mtimeMs < 3600e3) return selected;
  let staging;
  try {
    const tags = JSON.parse(await npm(["view", "boubou", "dist-tags", "--json", "--prefer-online", "--fetch-retries=0", "--fetch-timeout=5000"], 8000));
    if (!tags || typeof tags !== "object" || Array.isArray(tags) || !version(tags.latest)) throw new Error("npm returned invalid release tags");
    const newest = [tags.latest, tags[channel(selected.version)]]
      .filter((candidate) => eligible(candidate, selected.version))
      .sort(compare).at(-1);
    if (!newest || compare(newest, selected.version) <= 0) return selected;
    let destination = join(versions, newest);
    let directory = join(destination, "node_modules", "boubou");
    if (await ready(directory, newest)) return { version: newest, directory };
    if (!await agree(`Boubou ${newest} is available (you have ${selected.version}). Update now? [Y/n] `)) return selected;
    console.error(`Updating Boubou ${selected.version} → ${newest}…`);
    await fs.mkdir(versions, { recursive: true });
    staging = await fs.mkdtemp(join(versions, ".install-"));
    await npm(["install", "--prefix", staging, "--global=false", "--ignore-scripts", "--no-audit", "--no-fund", "--package-lock=false", "--save=false", "--engine-strict", "--fetch-retries=0", "--fetch-timeout=15000", `boubou@${newest}`], 120000);
    if (!await ready(join(staging, "node_modules", "boubou"), newest)) throw new Error("the downloaded package is incomplete");
    try { await fs.rename(staging, destination); }
    catch (error) {
      // A concurrent invocation may have published the same complete version first.
      if (!await ready(directory, newest)) {
        if (error.code !== "EEXIST" && error.code !== "ENOTEMPTY") throw error;
        // Preserve a broken cache entry and publish beside it; never remove a directory
        // another launcher may just have installed or a running process may still use.
        destination = `${destination}_${staging.split(sep).at(-1).slice(9)}`;
        await fs.rename(staging, destination);
        directory = join(destination, "node_modules", "boubou");
      }
    }
    return { version: newest, directory };
  } catch (error) {
    if (!shutdownSignal) console.error(`Boubou could not update (${error.message}); using ${selected.version}.`);
    return selected;
  } finally {
    if (staging) await fs.rm(staging, { recursive: true, force: true }).catch(() => {});
    await fs.mkdir(versions, { recursive: true }).then(() => fs.writeFile(checked, "")).catch(() => {});
  }
}

async function main() {
  if (!executable) throw new Error("Boubou's npm package runs on Apple Silicon Macs and x86-64 Linux.");
  const selected = process.env.BOUBOU_UPDATE_CHECKED === "1" ? { directory: __dirname } : await selectPackage();
  if (shutdownSignal) return finish(1);
  const folders = __dirname.split(sep);
  const source = process.env.BOUBOU_NPM || (folders.includes("_npx") ? "npx" : folders.at(-3) === "lib" ? "global" : undefined);
  const env = { ...process.env, ...(source && { BOUBOU_NPM: source }) };
  // Run the new launcher as well as the new binary; future launcher fixes take effect immediately.
  const replaced = selected.directory !== __dirname;
  activeChild = spawn(replaced ? process.execPath : join(__dirname, executable),
    replaced ? [join(selected.directory, "cli.cjs"), ...args] : args,
    { stdio: "inherit", env: { ...env,
      BOUBOU_UPDATE_CHECKED: selected.checked === false ? "0" : "1",
      ...(!replaced && { BOUBOU_NO_UPDATE_CHECK: "1" }),
    } });
  activeChild.once("error", (error) => {
    console.error(`Could not start boubou: ${error.message}`);
    finish(1);
  });
  activeChild.once("exit", finish);
}
main().catch((error) => { console.error(error.message); finish(1); });
