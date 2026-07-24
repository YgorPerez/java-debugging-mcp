#!/usr/bin/env bash
#
# Run the MCP-level integration tests: the real `jdwp-mcp` binary driven over JSON-RPC against real
# probe JVMs (mcp-server/tests/mcp_integration.rs). Each test compiles and launches its own probe
# from examples/probes/ and reaps it afterwards, so there are no manual steps.
#
# Requires a JDK (javac + java). Without one, every test prints SKIP and passes — so a green run on a
# JDK-less machine proves nothing; check the output for SKIP lines.
#
# Usage:
#   scripts/integration-test.sh                    # all of them
#   scripts/integration-test.sh force_return       # only tests whose name contains this
#   scripts/integration-test.sh -- --test-threads=1  # serial, easier to read when debugging
#
# The `--test mcp_integration` scope keeps the output to these tests and skips rebuilding the other
# harnesses; `cargo test -- --ignored` also works if you want everything.
set -euo pipefail

cd "$(dirname "$0")/.."

exec cargo test --test mcp_integration -- --ignored --nocapture "$@"
