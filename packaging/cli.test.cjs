const assert = require("node:assert/strict");
const { spawn } = require("node:child_process");
const fs = require("node:fs/promises");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");

const launcher = path.join(__dirname, "cli.cjs");
const native = `#!/usr/bin/env node
const fs = require("node:fs");
const path = require("node:path");
const root = process.platform === "linux" ? path.dirname(__dirname) : __dirname;
const result = {version: require(path.join(root, "package.json")).version, args: process.argv.slice(2), noCheck: process.env.BOUBOU_NO_UPDATE_CHECK};
if (process.env.FAKE_NATIVE_READY) {
  process.on("SIGINT", () => { fs.writeFileSync(process.env.FAKE_NATIVE_SIGNAL, "SIGINT"); process.exit(130); });
  fs.writeFileSync(process.env.FAKE_NATIVE_READY, "ready");
  setInterval(() => {}, 1000);
} else {
  console.log(JSON.stringify(result));
  process.exitCode = Number(process.env.FAKE_EXIT_CODE || 0);
}
`;
const npm = `#!/usr/bin/env node
const fs = require("node:fs");
const path = require("node:path");
const args = process.argv.slice(2);
fs.appendFileSync(process.env.FAKE_NPM_LOG, JSON.stringify(args) + "\\n");
if (args[0] === "view") {
  if (process.env.FAKE_VIEW_FAIL) process.exit(1);
  console.log(process.env.FAKE_TAGS);
} else if (args[0] === "install") {
  const prefix = args[args.indexOf("--prefix") + 1];
  if (!prefix) throw new Error("Missing installation prefix");
  const destination = path.join(prefix, "node_modules", "boubou");
  fs.mkdirSync(path.dirname(destination), {recursive: true});
  fs.cpSync(process.env.FAKE_NEW_PACKAGE, destination, {recursive: true});
  if (process.env.FAKE_INSTALL_READY) {
    fs.writeFileSync(process.env.FAKE_INSTALL_READY, "ready");
    setInterval(() => {}, 1000);
  } else if (process.env.FAKE_INSTALL_FAIL) {
    process.exitCode = 1;
  } else if (process.env.FAKE_INSTALL_DELAY) {
    setTimeout(() => {}, Number(process.env.FAKE_INSTALL_DELAY));
  }
} else throw new Error("Unexpected npm command: " + args.join(" "));
`;

async function packageAt(root, version) {
  await fs.mkdir(root, { recursive: true });
  await fs.copyFile(launcher, path.join(root, "cli.cjs"));
  await fs.writeFile(path.join(root, "package.json"), JSON.stringify({ name: "boubou", version }));
  for (const binary of ["boubou", "linux-x64/boubou"]) {
    await fs.mkdir(path.dirname(path.join(root, binary)), { recursive: true });
    await fs.writeFile(path.join(root, binary), native, { mode: 0o755 });
  }
}

async function fixture(t, version = "0.1.0-beta.10") {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "boubou-cli-"));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const original = path.join(root, "original");
  const newer = path.join(root, "newer");
  const bin = path.join(root, "bin");
  const cache = path.join(root, "cache");
  await packageAt(original, version);
  await packageAt(newer, "0.1.0-beta.11");
  await fs.mkdir(bin);
  await fs.writeFile(path.join(bin, "npm"), npm, { mode: 0o755 });
  const env = { ...process.env };
  for (const key of Object.keys(env)) {
    if (/^(npm_|NPM_|BOUBOU_|FAKE_)/.test(key) || key === "CI") delete env[key];
  }
  Object.assign(env, {
    HOME: root,
    XDG_CACHE_HOME: path.join(root, "xdg"),
    BOUBOU_HOME: cache,
    PATH: `${bin}${path.delimiter}${process.env.PATH}`,
    FAKE_NPM_LOG: path.join(root, "npm.log"),
    FAKE_NEW_PACKAGE: newer,
    FAKE_TAGS: JSON.stringify({ latest: "0.1.0-beta.11", beta: "0.1.0-beta.11" }),
  });
  function start(args = [], extra = {}) {
    const child = spawn(process.execPath, [path.join(original, "cli.cjs"), ...args], {
      env: { ...env, ...extra }, stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    const timer = setTimeout(() => child.kill("SIGKILL"), 15000);
    const done = new Promise((resolve, reject) => {
      child.once("error", reject);
      child.once("close", (code, signal) => {
        clearTimeout(timer);
        resolve({ code, signal, stdout, stderr });
      });
    });
    t.after(() => { clearTimeout(timer); if (child.exitCode === null) child.kill("SIGKILL"); });
    return { child, done };
  }
  const calls = async () => {
    const text = await fs.readFile(env.FAKE_NPM_LOG, "utf8").catch((e) => {
      if (e.code === "ENOENT") return "";
      throw e;
    });
    return text.trim() ? text.trim().split("\n").map(JSON.parse) : [];
  };
  return { root, cache, original, newer, env, start, calls, run: async (args, extra) => start(args, extra).done };
}

function result(run) {
  assert.equal(run.signal, null, run.stderr);
  assert.equal(run.code, 0, run.stderr);
  return JSON.parse(run.stdout.trim());
}

async function waitFor(file) {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    if (await fs.stat(file).then(() => true, () => false)) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error(`Timed out waiting for ${path.basename(file)}`);
}

// These tests execute the real launcher with a private npm and model-cache fixture.
// No test connects to npm, downloads a model, or modifies the user's installation.
test("upgrade executes the new CLI with unchanged arguments and preserves models", async (t) => {
  const f = await fixture(t);
  await fs.mkdir(path.join(f.cache, "models"), { recursive: true });
  const model = path.join(f.cache, "models", "weights.bin");
  await fs.writeFile(model, "already downloaded");
  const before = await fs.stat(model);
  const args = ["run", "model with spaces", "--input", "input's audio.wav"];
  assert.deepEqual(result(await f.run(args)), { version: "0.1.0-beta.11", args, noCheck: "1" });
  const calls = await f.calls();
  assert.deepEqual(calls.map((args) => args[0]), ["view", "install"]);
  assert.ok(calls[1].includes("boubou@0.1.0-beta.11"));
  assert.ok(calls[1].includes("--ignore-scripts"));
  assert.ok(calls[1].includes("--global=false"));
  assert.equal(await fs.readFile(model, "utf8"), "already downloaded");
  assert.equal((await fs.stat(model)).mtimeMs, before.mtimeMs);
  assert.equal(result(await f.run(["list"])).version, "0.1.0-beta.11");
  assert.deepEqual((await f.calls()).map((args) => args[0]), ["view", "install", "view"]);
});

test("offline and disabled checks reuse the newest completed version without npm", async (t) => {
  const f = await fixture(t);
  await packageAt(path.join(f.cache, "npm", "versions", "0.1.0-beta.11", "node_modules", "boubou"), "0.1.0-beta.11");
  for (const [args, env] of [
    [["run", "model", "--offline"], {}],
    [["doctor"], {}],
    [["list"], { BOUBOU_NO_UPDATE_CHECK: "1" }],
    [["list"], { npm_config_offline: "true" }],
  ]) assert.equal(result(await f.run(args, env)).version, "0.1.0-beta.11");
  assert.deepEqual(await f.calls(), []);
});

test("the newest cached launcher owns the next online update", async (t) => {
  const f = await fixture(t);
  const cached = path.join(f.cache, "npm", "versions", "0.1.0-beta.11", "node_modules", "boubou");
  await packageAt(cached, "0.1.0-beta.11");
  const cachedLauncher = path.join(cached, "cli.cjs");
  const source = await fs.readFile(cachedLauncher, "utf8");
  const marker = `process.env.FAKE_TAGS = ${JSON.stringify(JSON.stringify({ latest: "0.1.0-beta.12", beta: "0.1.0-beta.12" }))};`;
  await fs.writeFile(cachedLauncher, source.replace("\n", `\n${marker}\n`));
  await packageAt(f.newer, "0.1.0-beta.12");
  assert.equal(result(await f.run(["list"])).version, "0.1.0-beta.12");
  assert.deepEqual((await f.calls()).map((args) => args[0]), ["view", "install"]);
});

test("a corrupt committed version is replaced by a complete download", async (t) => {
  const f = await fixture(t);
  const cached = path.join(f.cache, "npm", "versions", "0.1.0-beta.11", "node_modules", "boubou");
  await packageAt(cached, "0.1.0-beta.10");
  assert.equal(result(await f.run(["list"])).version, "0.1.0-beta.11");
  assert.equal(result(await f.run(["list"], { BOUBOU_NO_UPDATE_CHECK: "1" })).version, "0.1.0-beta.11");
  assert.equal(result(await f.run(["list"])).version, "0.1.0-beta.11");
  assert.equal((await f.calls()).filter((args) => args[0] === "install").length, 1);
});

test("failed registry checks keep a completed upgrade usable", async (t) => {
  const f = await fixture(t);
  assert.equal(result(await f.run(["list"])).version, "0.1.0-beta.11");
  const failed = await f.run(["list"], { FAKE_VIEW_FAIL: "1" });
  assert.equal(result(failed).version, "0.1.0-beta.11");
  assert.ok(failed.stderr.length > 0, "a failed online update must be visible");
});

test("a failed install never activates its partly written package", async (t) => {
  const f = await fixture(t);
  const failed = await f.run(["list"], { FAKE_INSTALL_FAIL: "1" });
  assert.equal(result(failed).version, "0.1.0-beta.10");
  assert.ok(failed.stderr.length > 0);
  assert.equal(result(await f.run(["list"], { BOUBOU_NO_UPDATE_CHECK: "1" })).version, "0.1.0-beta.10");
});

test("an incomplete or mismatched installation is rejected", async (t) => {
  const f = await fixture(t);
  await fs.rm(path.join(f.newer, process.platform === "linux" ? "linux-x64/boubou" : "boubou"));
  assert.equal(result(await f.run(["list"])).version, "0.1.0-beta.10");
  await packageAt(f.newer, "0.1.0-beta.9");
  assert.equal(result(await f.run(["list"])).version, "0.1.0-beta.10");
});

test("malformed registry replies cannot become package paths or install arguments", async (t) => {
  const f = await fixture(t);
  for (const tags of ["<html>Sign in</html>", "null", JSON.stringify({ latest: "../../outside", beta: "9.0.0-\\u001b[2J" })]) {
    assert.equal(result(await f.run(["list"], { FAKE_TAGS: tags })).version, "0.1.0-beta.10");
  }
  assert.equal((await f.calls()).filter((args) => args[0] === "install").length, 0);
});

test("registry rollback cannot downgrade a completed upgrade", async (t) => {
  const f = await fixture(t);
  assert.equal(result(await f.run(["list"])).version, "0.1.0-beta.11");
  const old = JSON.stringify({ latest: "0.1.0-beta.9", beta: "0.1.0-beta.9" });
  assert.equal(result(await f.run(["list"], { FAKE_TAGS: old })).version, "0.1.0-beta.11");
  assert.equal((await f.calls()).filter((args) => args[0] === "install").length, 1);
});

test("a stable installation stays stable and beta users advance to the stable release", async (t) => {
  const f = await fixture(t, "0.1.0");
  await packageAt(path.join(f.cache, "npm", "versions", "0.2.0-beta.1", "node_modules", "boubou"), "0.2.0-beta.1");
  const tags = JSON.stringify({ latest: "0.1.0", beta: "0.2.0-beta.1" });
  assert.equal(result(await f.run(["list"], { FAKE_TAGS: tags })).version, "0.1.0");
  assert.equal((await f.calls()).filter((args) => args[0] === "install").length, 0);
  const beta = await fixture(t);
  await packageAt(beta.newer, "0.1.0");
  assert.equal(result(await beta.run(["list"], { FAKE_TAGS: JSON.stringify({ latest: "0.1.0", beta: "0.1.0-beta.11" }) })).version, "0.1.0");
});

test("concurrent launches activate only complete packages", async (t) => {
  const f = await fixture(t);
  const runs = await Promise.all([
    f.run(["list"], { FAKE_INSTALL_DELAY: "120" }),
    f.run(["list"], { FAKE_INSTALL_DELAY: "120" }),
  ]);
  for (const run of runs) assert.equal(result(run).version, "0.1.0-beta.11");
  assert.equal(result(await f.run(["list"], { BOUBOU_NO_UPDATE_CHECK: "1" })).version, "0.1.0-beta.11");
});

test("--cache-dir takes precedence over BOUBOU_HOME for the code cache", async (t) => {
  const f = await fixture(t);
  const other = path.join(f.root, "cache with spaces");
  assert.equal(result(await f.run(["--cache-dir", other, "list"])).version, "0.1.0-beta.11");
  await fs.access(path.join(other, "npm", "versions", "0.1.0-beta.11", "node_modules", "boubou", "cli.cjs"));
  assert.equal(result(await f.run(["list"], { BOUBOU_NO_UPDATE_CHECK: "1" })).version, "0.1.0-beta.10");
});

test("native exit status and shutdown signals survive both launchers after an upgrade", async (t) => {
  const f = await fixture(t);
  await packageAt(path.join(f.cache, "npm", "versions", "0.1.0-beta.11", "node_modules", "boubou"), "0.1.0-beta.11");
  assert.equal((await f.run(["list"], { BOUBOU_NO_UPDATE_CHECK: "1", FAKE_EXIT_CODE: "23" })).code, 23);
  const ready = path.join(f.root, "native-ready");
  const signal = path.join(f.root, "native-signal");
  const run = f.start(["list"], { BOUBOU_NO_UPDATE_CHECK: "1", FAKE_NATIVE_READY: ready, FAKE_NATIVE_SIGNAL: signal });
  await waitFor(ready);
  run.child.kill("SIGTERM");
  assert.equal((await run.done).signal, "SIGTERM");
  assert.equal(await fs.readFile(signal, "utf8"), "SIGINT");
});

test("interrupting an install cannot start the command or activate the staged version", async (t) => {
  const f = await fixture(t);
  const ready = path.join(f.root, "install-ready");
  const run = f.start(["list"], { FAKE_INSTALL_READY: ready });
  await waitFor(ready);
  run.child.kill("SIGINT");
  const interrupted = await run.done;
  assert.equal(interrupted.signal, "SIGINT");
  assert.equal(interrupted.stdout, "");
  assert.equal(result(await f.run(["list"], { BOUBOU_NO_UPDATE_CHECK: "1" })).version, "0.1.0-beta.10");
});
