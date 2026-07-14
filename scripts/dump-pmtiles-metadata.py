#!/usr/bin/env python3
"""Dump the raw metadata JSON from a PMTiles v3 archive.

`elivagar inspect` renders only the fields it knows about (layer list, zoom
range), so it cannot show extension members such as `elivagar` or
`ocean_artifact`. This prints the metadata section verbatim.

The metadata section is located by two u64 LE header fields at byte offsets 24
(offset) and 32 (length), and is compressed with the archive's internal
compression, which elivagar always writes as gzip.

Usage: scripts/dump-pmtiles-metadata.py <archive.pmtiles> [--key KEY]
"""

import gzip
import json
import struct
import sys

METADATA_OFFSET_FIELD = 24
METADATA_LENGTH_FIELD = 32


def read_metadata(path):
    with open(path, "rb") as fh:
        header = fh.read(127)
        if header[:7] != b"PMTiles":
            raise SystemExit(f"{path}: not a PMTiles archive")
        (offset,) = struct.unpack_from("<Q", header, METADATA_OFFSET_FIELD)
        (length,) = struct.unpack_from("<Q", header, METADATA_LENGTH_FIELD)
        fh.seek(offset)
        raw = fh.read(length)
    return json.loads(gzip.decompress(raw))


def main():
    args = [a for a in sys.argv[1:]]
    if not args:
        raise SystemExit(__doc__)
    path = args[0]
    key = None
    if "--key" in args:
        key = args[args.index("--key") + 1]
    meta = read_metadata(path)
    if key is not None:
        if key not in meta:
            raise SystemExit(f"{path}: no {key!r} member (keys: {sorted(meta)})")
        meta = meta[key]
    json.dump(meta, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
