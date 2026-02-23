#!/usr/bin/env python3
"""Read PMTiles v3 header and directory to print per-zoom tile stats."""

import struct
import sys
import gzip
import zlib

def decompress(data, compression_type):
    """Decompress data based on PMTiles compression type."""
    if compression_type == 1:  # none
        return data
    elif compression_type == 2:  # gzip
        return gzip.decompress(data)
    elif compression_type == 4:  # zstd
        try:
            import zstandard
            return zstandard.ZstdDecompressor().decompress(data)
        except ImportError:
            # Try raw decompression as fallback
            print("  WARNING: zstd not available, trying raw inflate")
            return zlib.decompress(data, -15)
    else:
        # Try gzip first, then raw
        try:
            return gzip.decompress(data)
        except Exception:
            return data

def read_varint(data, pos):
    """Read a protobuf-style varint."""
    result = 0
    shift = 0
    while True:
        b = data[pos]
        pos += 1
        result |= (b & 0x7F) << shift
        if (b & 0x80) == 0:
            break
        shift += 7
    return result, pos

def decode_directory(data):
    """Decode a PMTiles v3 directory into (tile_id, run_length, length, offset) entries."""
    pos = 0
    count, pos = read_varint(data, pos)

    # Column 1: delta-encoded tile IDs
    tile_ids = []
    prev = 0
    for _ in range(count):
        delta, pos = read_varint(data, pos)
        prev += delta
        tile_ids.append(prev)

    # Column 2: run lengths
    run_lengths = []
    for _ in range(count):
        rl, pos = read_varint(data, pos)
        run_lengths.append(rl)

    # Column 3: lengths
    lengths = []
    for _ in range(count):
        length, pos = read_varint(data, pos)
        lengths.append(length)

    # Column 4: offsets
    offsets = []
    running_offset = 0
    for i in range(count):
        val, pos = read_varint(data, pos)
        if val == 0 and i > 0:
            running_offset += lengths[i - 1]
            offsets.append(running_offset)
        else:
            running_offset = val - 1
            offsets.append(running_offset)

    return list(zip(tile_ids, run_lengths, lengths, offsets))

def tile_id_to_z(tile_id):
    """Get zoom level from a PMTiles Hilbert tile ID."""
    if tile_id == 0:
        return 0
    z = 0
    while True:
        z += 1
        n = 1 << z
        next_base = (n * n * 4 - 1) // 3
        if tile_id < next_base or z >= 31:
            break
    return z

def main():
    if len(sys.argv) < 2:
        print("Usage: pmtiles-stats.py <file.pmtiles> [file2.pmtiles ...]")
        sys.exit(1)

    for path in sys.argv[1:]:
        print(f"\n=== {path} ===")
        with open(path, 'rb') as f:
            header = f.read(127)

        magic = header[:7]
        version = header[7]
        if magic != b'PMTiles' or version != 3:
            print(f"  Not a PMTiles v3 file (magic={magic}, version={version})")
            continue

        root_dir_offset = struct.unpack_from('<Q', header, 8)[0]
        root_dir_length = struct.unpack_from('<Q', header, 16)[0]
        leaf_dirs_offset = struct.unpack_from('<Q', header, 40)[0]
        leaf_dirs_length = struct.unpack_from('<Q', header, 48)[0]
        data_offset = struct.unpack_from('<Q', header, 56)[0]
        data_length = struct.unpack_from('<Q', header, 64)[0]
        num_addressed = struct.unpack_from('<Q', header, 72)[0]
        num_entries = struct.unpack_from('<Q', header, 80)[0]
        num_unique = struct.unpack_from('<Q', header, 88)[0]
        internal_compression = header[97]
        min_zoom = header[100]
        max_zoom = header[101]

        comp_names = {0: 'unknown', 1: 'none', 2: 'gzip', 3: 'brotli', 4: 'zstd'}
        comp_name = comp_names.get(internal_compression, f'#{internal_compression}')
        print(f"  Zoom: z{min_zoom}-z{max_zoom}, internal compression: {comp_name}")
        print(f"  Addressed: {num_addressed:,}, Entries: {num_entries:,}, Unique: {num_unique:,}")
        print(f"  Data size: {data_length:,} bytes ({data_length / 1024 / 1024:.1f} MB)")

        # Read and decompress root directory
        with open(path, 'rb') as f:
            f.seek(root_dir_offset)
            root_compressed = f.read(root_dir_length)

        root_data = decompress(root_compressed, internal_compression)

        root_entries = decode_directory(root_data)

        # Check if there are leaf directories
        all_entries = []
        if leaf_dirs_length > 0:
            with open(path, 'rb') as f:
                f.seek(leaf_dirs_offset)
                leaf_blob = f.read(leaf_dirs_length)

            for tile_id, run_length, length, offset in root_entries:
                if run_length == 0:
                    # Leaf pointer
                    leaf_compressed = leaf_blob[offset:offset + length]
                    leaf_data = decompress(leaf_compressed, internal_compression)
                    leaf_entries = decode_directory(leaf_data)
                    all_entries.extend(leaf_entries)
                else:
                    # Regular tile entry in root
                    all_entries.append((tile_id, run_length, length, offset))
        else:
            all_entries = root_entries

        # Count tiles per zoom
        tiles_per_zoom = {}
        unique_offsets_per_zoom = {}
        for tile_id, run_length, length, offset in all_entries:
            z = tile_id_to_z(tile_id)
            rl = max(run_length, 1)
            tiles_per_zoom[z] = tiles_per_zoom.get(z, 0) + rl
            if z not in unique_offsets_per_zoom:
                unique_offsets_per_zoom[z] = set()
            unique_offsets_per_zoom[z].add((offset, length))

        print(f"  Directory entries: {len(all_entries):,}")
        print(f"  Per-zoom tiles:")
        total_tiles = 0
        for z in sorted(tiles_per_zoom.keys()):
            count = tiles_per_zoom[z]
            unique = len(unique_offsets_per_zoom.get(z, set()))
            total_tiles += count
            print(f"    z{z:2}: {count:>8} tiles, {unique:>8} unique offsets")
        print(f"  Total: {total_tiles:,} tiles")

if __name__ == '__main__':
    main()
