#!/usr/bin/env node
// The `npx jdwp-mcp` entry point (REL-6, #168).
//
// It resolves the prebuilt binary out of whichever `jdwp-mcp-<os>-<cpu>` package npm installed for this
// machine and execs it. There is deliberately NO download here: the binary arrives as an
// `optionalDependencies` package, so `npm install` fetches exactly one of the five and `npx` needs no
// network at run time. A `postinstall` fetch is the usual alternative and is what makes `npx` unreliable
// in CI — behind a proxy, on an offline runner, or against a rate-limited GitHub it fails at the moment
// somebody is trying to start a debugger.
//
// STDIO IS THE MCP TRANSPORT, which is the constraint the whole file is arranged around. Nothing here may
// print to stdout on the happy path — a single stray line corrupts the JSON-RPC stream, and the failure
// looks like a broken server rather than a chatty launcher. Every message below goes to stderr.

"use strict";

const { spawnSync } = require("node:child_process");

// `process.platform` / `process.arch` are npm's own `os` / `cpu` vocabulary, which is why the package
// names are built from them rather than from Rust target triples: the two disagree (`win32` vs
// `pc-windows-msvc`, `x64` vs `x86_64`) and translating in two directions is a table to get wrong.
//
// ONE PACKAGE NAME DIVERGES FROM THAT RULE, AND IT IS A REGISTRY OWNERSHIP FACT RATHER THAN A TASTE ONE
// (REL-10, #194). The rule above produces `jdwp-mcp-win32-x64`; npm's security-holder account took that name
// on 2026-08-12 and it can no longer be published to, so the Windows package is `jdwp-mcp-windows-x64`.
// **Do not "fix" this back to the derived name** — it reads like an inconsistency and it is the only name
// here that npm will not accept. `platformKey` below stays in npm's vocabulary, because that half really is
// `process.platform` and is compared against it.
const PACKAGE_OS = { win32: "windows" };
const platformKey = `${process.platform}-${process.arch}`;
const pkg = `jdwp-mcp-${PACKAGE_OS[process.platform] ?? process.platform}-${process.arch}`;
const binary = process.platform === "win32" ? "jdwp-mcp.exe" : "jdwp-mcp";

let resolved;
try {
  resolved = require.resolve(`${pkg}/${binary}`);
} catch {
  // Two different failures reach here and they need different advice, so say which is which rather than
  // printing "not found" over both.
  // PLATFORM KEYS, not package names — `win32-x64` is what `process.platform`/`process.arch` report on
  // Windows, and this list is compared against exactly that. The package it maps to is
  // `jdwp-mcp-windows-x64`; see `PACKAGE_OS` above for why the two differ.
  const supported = [
    "linux-x64",
    "linux-arm64",
    "darwin-arm64",
    "darwin-x64",
    "win32-x64",
  ];
  const known = supported.includes(platformKey);
  process.stderr.write(
    known
      ? `jdwp-mcp: ${pkg} is not installed, though this platform is supported.\n` +
          `  Two things cause that, and they need different fixes:\n` +
          `    1. The install skipped optional dependencies (--no-optional / --omit=optional).\n` +
          `       Reinstall without that flag.\n` +
          `    2. ${pkg} is not on the registry for this version. Some versions ship a subset of\n` +
          `       platforms; check https://www.npmjs.com/package/${pkg} and try a newer jdwp-mcp.\n` +
          `  Either way this always works, and is fully supported:\n` +
          `    cargo install jdwp-mcp\n`
      : `jdwp-mcp: no prebuilt binary for ${platformKey}.\n` +
          `  Prebuilt: ${supported.join(", ")}.\n` +
          `  Everything else builds from source and is fully supported that way:\n` +
          `    cargo install jdwp-mcp\n`,
  );
  process.exit(1);
}

// `spawnSync` with `stdio: "inherit"` rather than `execve`: Node has no exec that replaces the process,
// and inheriting all three descriptors is what keeps this transparent to an MCP client — the server owns
// stdin and stdout directly, with this process only forwarding the exit status.
const run = spawnSync(resolved, process.argv.slice(2), { stdio: "inherit" });

if (run.error) {
  process.stderr.write(`jdwp-mcp: could not start ${resolved}: ${run.error.message}\n`);
  process.exit(1);
}
// A signal death has no exit code. Reporting 1 for it would call a SIGTERM a failure of the server, so
// the conventional 128+n is used instead and the signal is named.
if (run.signal) {
  process.stderr.write(`jdwp-mcp: the server was terminated by ${run.signal}.\n`);
  process.exit(1);
}
process.exit(run.status === null ? 1 : run.status);
