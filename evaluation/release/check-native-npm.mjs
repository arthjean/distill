#!/usr/bin/env bun

import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../..", import.meta.url));
const [target, destinationArgument] = process.argv.slice(2);
const supportedTargets = new Set(["linux-x86_64", "macos-arm64"]);

function fail(message) {
  throw new Error(message);
}

if (!supportedTargets.has(target) || !destinationArgument) {
  fail(
    "usage: check-native-npm.mjs <linux-x86_64|macos-arm64> <destination>",
  );
}

const actualTarget =
  process.platform === "linux" && process.arch === "x64"
    ? "linux-x86_64"
    : process.platform === "darwin" && process.arch === "arm64"
      ? "macos-arm64"
      : `${process.platform}-${process.arch}`;
if (actualTarget !== target) {
  fail(`target ${target} cannot execute on ${actualTarget}`);
}

const packageSource = join(root, "npm/distill");
const packageJson = JSON.parse(
  readFileSync(join(packageSource, "package.json"), "utf8"),
);
if (
  packageJson.name !== "@arthjean/distill" ||
  packageJson.version !== "0.1.0" ||
  packageJson.bin?.distill !== "bin/distill" ||
  JSON.stringify(packageJson.os) !== JSON.stringify(["linux", "darwin"]) ||
  Object.hasOwn(packageJson, "cpu") ||
  Object.hasOwn(packageJson, "scripts") ||
  JSON.stringify(packageJson.files) !==
    JSON.stringify(["bin", "vendor", "README.md", "LICENSE"])
) {
  fail("npm package metadata violates the preregistered release contract");
}

const launcherSource = join(packageSource, "bin/distill");
if ((lstatSync(launcherSource).mode & 0o111) === 0) {
  fail("npm launcher is not executable");
}

const destination = resolve(destinationArgument);
mkdirSync(join(destination, "package/bin"), { recursive: true, mode: 0o700 });
mkdirSync(join(destination, "package/vendor/linux-x86_64"), {
  recursive: true,
  mode: 0o700,
});
mkdirSync(join(destination, "package/vendor/macos-arm64"), {
  recursive: true,
  mode: 0o700,
});
mkdirSync(join(destination, "shim"), { recursive: true, mode: 0o700 });
mkdirSync(join(destination, "fake-arch"), { recursive: true, mode: 0o700 });
mkdirSync(join(destination, "fake-libc"), { recursive: true, mode: 0o700 });

const launcher = join(destination, "package/bin/distill");
copyFileSync(launcherSource, launcher);
chmodSync(launcher, 0o755);

for (const fixtureTarget of supportedTargets) {
  const fixture = join(
    destination,
    `package/vendor/${fixtureTarget}/distill`,
  );
  writeFileSync(
    fixture,
    `#!/bin/sh\nprintf 'selected:${fixtureTarget}\\n'\nprintf 'arg:%s\\n' "$@"\n`,
    { mode: 0o700 },
  );
}

const shim = join(destination, "shim/distill");
symlinkSync("../package/bin/distill", shim);
const selected = spawnSync(shim, ["alpha", "beta gamma"], {
  encoding: "utf8",
  env: process.env,
});
if (
  selected.status !== 0 ||
  selected.stdout !== `selected:${target}\narg:alpha\narg:beta gamma\n` ||
  selected.stderr !== ""
) {
  fail("npm launcher did not select the qualified platform executable");
}

const unsupportedOs = target === "linux-x86_64" ? "Linux" : "Darwin";
const unsupportedArch = target === "linux-x86_64" ? "aarch64" : "x86_64";
const fakeUname = join(destination, "fake-arch/uname");
writeFileSync(
  fakeUname,
  `#!/bin/sh\ncase "$1" in\n  -s) printf '${unsupportedOs}\\n' ;;\n  -m) printf '${unsupportedArch}\\n' ;;\n  *) exit 2 ;;\nesac\n`,
  { mode: 0o700 },
);
const rejected = spawnSync(shim, [], {
  encoding: "utf8",
  env: {
    ...process.env,
    PATH: `${join(destination, "fake-arch")}:${process.env.PATH ?? ""}`,
  },
});
const rejection =
  `distill: unsupported platform ${unsupportedOs} ${unsupportedArch}\n` +
  "supported platforms: Linux x86_64, macOS arm64\n";
if (rejected.status !== 2 || rejected.stdout !== "" || rejected.stderr !== rejection) {
  fail("npm launcher did not reject the unsupported architecture explicitly");
}

if (target === "linux-x86_64") {
  const fakeGetconf = join(destination, "fake-libc/getconf");
  writeFileSync(fakeGetconf, "#!/bin/sh\nexit 1\n", { mode: 0o700 });
  const rejectedLibc = spawnSync(shim, [], {
    encoding: "utf8",
    env: {
      ...process.env,
      PATH: `${join(destination, "fake-libc")}:${process.env.PATH ?? ""}`,
    },
  });
  if (
    rejectedLibc.status !== 2 ||
    rejectedLibc.stdout !== "" ||
    rejectedLibc.stderr !==
      "distill: unsupported Linux x86_64 runtime (GNU libc required)\n"
  ) {
    fail("npm launcher did not reject a non-GNU Linux runtime explicitly");
  }
}

process.stdout.write(`${target} npm package surface: PASS\n`);
