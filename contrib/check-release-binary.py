#!/usr/bin/env python3
"""Reject production binaries that contain development-only ACP fixture markers."""

from pathlib import Path
import sys


MARKERS = (
    "Mock Echo",
    "mock-acp-agent.js",
    "mock ACP agent",
    "mock-session-",
    "mock_acp",
)


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {Path(sys.argv[0]).name} <binary>", file=sys.stderr)
        return 2

    binary = Path(sys.argv[1])
    data = binary.read_bytes()
    found = [
        marker
        for marker in MARKERS
        if marker.encode("ascii") in data or marker.encode("utf-16le") in data
    ]
    if found:
        print(
            f"{binary} contains development-only ACP markers: {', '.join(found)}",
            file=sys.stderr,
        )
        return 1

    print(f"{binary}: no development-only ACP markers")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
