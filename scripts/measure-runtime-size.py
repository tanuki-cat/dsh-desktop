#!/usr/bin/env python3
"""Report how big a staged runtime is, raw and compressed.

Usage: measure-runtime-size.py <runtime dir> [--json]

Prints one row per top-level directory plus extension totals, using file bytes
for the raw size and per-file zlib deflate (level 6) as a zip approximation.
The real artifact adds roughly 100 bytes per file of zip headers, which the
summary calls out. du block occupancy is deliberately not used: it overstates
a tree full of small files by 5-10% and hides the payload that matters.
"""

import argparse
import collections
import json
import os
import sys
import zlib
from concurrent.futures import ProcessPoolExecutor

EXT_GROUPS = [
    (("types",), (".d.ts", ".d.cts", ".d.mts")),
    (("ts",), (".ts", ".tsx", ".mts", ".cts")),
    (("js",), (".js", ".cjs", ".mjs")),
    (("native",), (".node", ".dylib", ".so", ".wasm", ".data", ".exe")),
    (("maps",), (".map",)),
    (("pdb",), (".pdb",)),
    (("md",), (".md",)),
    (("json",), (".json",)),
]


def group_of(name):
    lower = name.lower()
    for names, suffixes in EXT_GROUPS:
        if lower.endswith(suffixes):
            return names[0]
    return "other"


def one(path):
    try:
        raw = 0
        packed = zlib.compressobj(6, zlib.DEFLATED, -15)
        size = 0
        with open(path, "rb") as handle:
            while True:
                chunk = handle.read(1 << 20)
                if not chunk:
                    break
                raw += len(chunk)
                size += len(packed.compress(chunk))
        size += len(packed.flush())
        return path, raw, size
    except OSError:
        return path, 0, 0


def walk(root):
    paths = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(filenames):
            path = os.path.join(dirpath, name)
            if not os.path.islink(path):
                paths.append(path)
    return paths


def main():
    parser = argparse.ArgumentParser(description="Measure a staged runtime tree.")
    parser.add_argument("runtime")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    parser.add_argument("--workers", type=int, default=min(8, os.cpu_count() or 1))
    args = parser.parse_args()

    root = os.path.abspath(args.runtime)
    if not os.path.isdir(root):
        print("{} is not a directory".format(root), file=sys.stderr)
        return 2

    paths = walk(root)
    with ProcessPoolExecutor(max_workers=args.workers) as pool:
        results = list(pool.map(one, paths, chunksize=64))

    dirs = collections.defaultdict(lambda: {"files": 0, "raw": 0, "zip": 0})
    groups = collections.defaultdict(lambda: {"files": 0, "raw": 0, "zip": 0})
    for path, raw, packed in results:
        rel = os.path.relpath(path, root)
        top = rel.split(os.sep)[0]
        for bucket, key in ((dirs, top), (groups, group_of(os.path.basename(path)))):
            bucket[key]["files"] += 1
            bucket[key]["raw"] += raw
            bucket[key]["zip"] += packed

    total_files = sum(item["files"] for item in dirs.values())
    total_raw = sum(item["raw"] for item in dirs.values())
    total_zip = sum(item["zip"] for item in dirs.values())
    payload = {"root": root, "files": total_files, "raw": total_raw, "zip": total_zip,
               "dirs": dirs, "groups": groups}

    if args.json:
        print(json.dumps(payload, indent=1, sort_keys=True))
        return 0

    print("{}".format(root))
    print("{:22s} {:>8s} {:>12s} {:>12s}".format("directory", "files", "raw MB", "zip MB"))
    for name in sorted(dirs, key=lambda key: -dirs[key]["raw"]):
        item = dirs[name]
        print("{:22s} {:>8d} {:>12.1f} {:>12.1f}".format(
            name, item["files"], item["raw"] / 1048576, item["zip"] / 1048576))
    print("{:22s} {:>8d} {:>12.1f} {:>12.1f}".format(
        "total", total_files, total_raw / 1048576, total_zip / 1048576))
    print("")
    print("by kind:")
    for name in sorted(groups, key=lambda key: -groups[key]["raw"]):
        item = groups[name]
        print("  {:8s} {:>8d} files {:>10.1f} MB raw {:>10.1f} MB zip".format(
            name, item["files"], item["raw"] / 1048576, item["zip"] / 1048576))
    print("")
    print("artifact estimate: {:.0f} MB (zip + ~{:.0f} MB of per-file headers)".format(
        total_zip / 1048576 + total_files * 100 / 1048576, total_files * 100 / 1048576))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
