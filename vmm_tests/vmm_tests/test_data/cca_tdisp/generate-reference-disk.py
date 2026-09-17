# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Reproduce the reference guest's 64 MiB patterned disk without overwriting."""

import argparse
import hashlib
from pathlib import Path


EXPECTED_SHA256 = "281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    block = bytes(range(256)) * 4096
    with args.output.open("xb") as output:
        for _ in range(64):
            output.write(block)
    with args.output.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual != EXPECTED_SHA256:
        raise RuntimeError(f"reference disk hash mismatch: {actual}")
    print(f"{actual}  {args.output}")


if __name__ == "__main__":
    main()
