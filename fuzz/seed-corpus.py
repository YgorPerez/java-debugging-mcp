#!/usr/bin/env python3
"""Rebuild the fuzz corpora from the cassettes (TEST-45, #153).

    fuzz/seed-corpus.py                     # rebuild all three corpora
    cd fuzz && cargo +nightly fuzz run reply_packet

WHY THE CORPUS IS GENERATED RATHER THAN COMMITTED. It is derived from
`mcp-server/tests/cassettes/*.json`, which are re-recordable (`JDWP_RERECORD_CASSETTES=1`). A committed
copy would be a second representation of the same bytes, free to drift from the cassettes the moment one
is re-recorded — the duplicated-fact problem DOC-15 (#145) is about. Regenerating takes milliseconds.

WHY THE CASSETTES ARE THE RIGHT SEED. They are real JDWP replies from a real JVM, so a mutation starts
from a frame that already parses and changes one thing about it. Random bytes spend almost all their time
being rejected by the length check in the first eleven bytes; a mutated valid frame gets past it and
reaches the code that decides what the payload MEANS, which is where a desync would come from.
"""

import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CASSETTES = ROOT / "mcp-server" / "tests" / "cassettes"
CORPUS = ROOT / "fuzz" / "corpus"

# The JDWP reply header: 4-byte length, 4-byte id, the 0x80 reply flag, then a 2-byte error code.
HEADER_SIZE = 11
REPLY_FLAG = 0x80


def main() -> int:
    cassettes = sorted(CASSETTES.glob("*.json"))
    if not cassettes:
        print(f"no cassettes under {CASSETTES}", file=sys.stderr)
        return 1

    targets = {name: CORPUS / name for name in ("reply_packet", "value_by_tag", "read_string")}
    for directory in targets.values():
        directory.mkdir(parents=True, exist_ok=True)

    written = 0
    for cassette in cassettes:
        exchanges = json.loads(cassette.read_text()).get("exchanges", [])
        for index, exchange in enumerate(exchanges):
            reply = exchange.get("reply")
            if not reply:
                continue
            data = bytes.fromhex("".join(reply))
            stem = f"{cassette.stem}-{index:03d}"

            # `read_string` and `read_value_by_tag` are handed the command-specific payload as it
            # arrives, with no header in front of it.
            (targets["read_string"] / stem).write_bytes(data)
            (targets["value_by_tag"] / stem).write_bytes(data)

            # `ReplyPacket::decode` is handed a whole framed packet, so one has to be built around the
            # payload. The id is synthetic and the error code is zero: a recorded reply is a reply that
            # succeeded, and the interesting error codes are what the fuzzer will reach on its own.
            header = (
                (HEADER_SIZE + len(data)).to_bytes(4, "big")
                + (index + 1).to_bytes(4, "big")
                + bytes([REPLY_FLAG])
                + (0).to_bytes(2, "big")
            )
            (targets["reply_packet"] / stem).write_bytes(header + data)
            written += 1

    print(f"seeded {written} exchange(s) from {len(cassettes)} cassette(s):")
    for name, directory in sorted(targets.items()):
        print(f"  {name}: {len(list(directory.iterdir()))} file(s) in {directory.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
