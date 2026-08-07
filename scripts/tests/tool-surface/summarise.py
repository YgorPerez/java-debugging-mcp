#!/usr/bin/env python3
"""Reduce a tool-surface document to the facts worth pinning, for scripts/tests/run.sh.

The document is ~280KB of prose. Pinning it whole would mean regenerating an expected file on every
description edit, which is the DOC-7 (#108) failure — people regenerate without reading — and TEST-46
(#154) settled the shape for this repo: fragments over a fixed input table, not whole outputs.

So what is pinned is the SKELETON: the format's own identity, the two counts, and the internal-consistency
facts a half-parse would break. The counts moving IS a caller-visible change and belongs in a diff; the
description text moving is already guarded by the snapshot tests this document is built from.

Reads the document on stdin.
"""

import json
import sys


def main() -> int:
    document = json.load(sys.stdin)
    tools = document["tools"]
    names = [tool["name"] for tool in tools]
    print(f"{document['kind']} v{document['surface_version']} for {document['release']}")
    print(f"$schema: {document['$schema']}")
    print(f"{document['tool_count']} tools, {document['argument_count']} arguments")
    print(f"tool_count agrees with len(tools): {document['tool_count'] == len(tools)}")
    print(
        "argument_count agrees with the tools: "
        f"{document['argument_count'] == sum(len(t['arguments']) for t in tools)}"
    )
    print(f"every tool has a non-empty description: {all(t['description'] for t in tools)}")
    print(f"every argument has a schema object: {all(isinstance(a['schema'], dict) for t in tools for a in t['arguments'])}")
    print(f"tools are sorted by name: {names == sorted(names)}")
    print(f"first and last: {names[0]} … {names[-1]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
