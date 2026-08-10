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
const pkg = `jdwp-mcp-${process.platform}-${process.arch}`;
const binary = process.platform === "win32" ? "jdwp-mcp.exe" : "jdwp-mcp";

let resolved;
try {
  resolved = require.resolve(`${pkg}/${binary}`);
} catch {
  // Two different failures reach here and they need different advice, so say which is which rather than
  // printing "not found" over both.
  const supported = [
    "linux-x64",
    "linux-arm64",
    "darwin-arm64",
    "darwin-x64",
    "win32-x64",
  ];
  const known = supported.includes(`${process.platform}-${process.arch}`);
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
      : `jdwp-mcp: no prebuilt binary for ${process.platform}-${process.arch}.\n` +
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
