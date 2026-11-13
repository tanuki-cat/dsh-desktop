#!/usr/bin/env python3
"""Remove runtime payload that the bundled dsh never loads at run time.

Usage: prune-runtime.py <runtime dir> [--platform darwin|win32|linux] [--arch arm64|x64]

Rules (see docs/design-task-fix-bundled-runtime-size.md section 5.1):

  R1  type declarations (*.d.ts, *.d.cts, *.d.mts) not reachable from a runtime export
  R2  TypeScript sources (*.ts, *.mts, *.cts, *.tsx) not reachable from a runtime export
  R3  Windows debug symbols (*.pdb)
  R4  prebuilds / native payload for platforms and architectures other than the target
  R5  Node headers (node/include), corepack and share/doc|man
  R6  pnpm's duplicate platform binary (the launcher and @pnpm/exe-* are one file)
  R7  LibreOffice wasm engine on hosts that ship the native engine (not on Linux)
  R8  source maps (*.map)

Every deletion is reported per rule. The script is idempotent: running it twice
prints zeros the second time. It refuses to run on a directory that does not
look like a staging tree, so a typo cannot delete a user's installation.

Switches:
  DSH_RUNTIME_KEEP_NODE_HEADERS=1   keep node/include (offline node-gyp builds)
  DSH_RUNTIME_KEEP_TS=1             keep type declarations and TypeScript sources
"""

import argparse
import json
import os
import platform as host_platform
import shutil
import sys

TYPE_SUFFIXES = (".d.ts", ".d.cts", ".d.mts")
SOURCE_SUFFIXES = (".ts", ".tsx", ".mts", ".cts")
SKIP_CONDITIONS = {"types", "typings", "source", "@zod/source", "browser", "development"}
PLATFORM_TOKENS = ("darwin", "win32", "linux", "freebsd", "openbsd", "netbsd", "android", "aix")

RULES = [
    ("R1", "type declarations"),
    ("R2", "typescript sources"),
    ("R3", "windows debug symbols"),
    ("R4", "foreign-platform prebuilds"),
    ("R5", "node headers/corepack"),
    ("R6", "duplicate pnpm binary"),
    ("R7", "libreoffice wasm engine"),
    ("R8", "source maps"),
]


def normalize_platform(value):
    value = (value or "").lower()
    if value in ("win", "windows", "win32", "mingw", "msys", "cygwin"):
        return "win32"
    if value in ("mac", "macos", "darwin"):
        return "darwin"
    return value or "linux"


def normalize_arch(value):
    value = (value or "").lower()
    return {"aarch64": "arm64", "arm64": "arm64", "x86_64": "x64", "amd64": "x64", "x64": "x64"}.get(value, value)


def host_platform_name():
    if sys.platform == "darwin":
        return "darwin"
    if sys.platform.startswith("win"):
        return "win32"
    return "linux"


def human(size):
    return "{:.1f} MB".format(size / 1048576)


def walk_files(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(filenames):
            path = os.path.join(dirpath, name)
            if os.path.islink(path):
                continue
            yield path


def runtime_entry_keeper(root):
    """Paths a package manifest points at with a Node runtime condition.

    Anything a package resolves as import/require/default/main/bin has to
    survive. types, source and the like are build-time conditions Node never
    selects, so files reachable only through them are safe to remove.
    """
    keep = set()
    manifests = []
    for dirpath, dirnames, filenames in os.walk(root):
        if "package.json" in filenames:
            manifests.append(dirpath)

    def is_ts(path):
        return path.endswith(TYPE_SUFFIXES) or path.endswith(SOURCE_SUFFIXES)

    def visit(node, condition, base):
        if isinstance(node, str):
            candidate = os.path.normpath(os.path.join(base, node))
            if is_ts(candidate) and condition not in SKIP_CONDITIONS:
                keep.add(candidate)
        elif isinstance(node, dict):
            for key, value in node.items():
                visit(value, key, base)
        elif isinstance(node, list):
            for value in node:
                visit(value, condition, base)

    for base in manifests:
        try:
            with open(os.path.join(base, "package.json"), encoding="utf-8") as handle:
                manifest = json.load(handle)
        except (OSError, ValueError):
            continue
        if not isinstance(manifest, dict):
            continue
        for field in ("main", "module", "bin"):
            visit(manifest.get(field), field, base)
        visit(manifest.get("exports"), None, base)
    return keep


def foreign_payload(name, target_platform, target_arch):
    """Whether a directory name is another platform's payload."""
    for token in PLATFORM_TOKENS:
        if not name.startswith(token + "-"):
            continue
        rest = name[len(token) + 1:]
        return token != target_platform or not rest.startswith(target_arch)
    return False


def collect(root, target_platform, target_arch):
    """Classify every removable file / directory into the eight rules."""
    files = {key: [] for key, _ in RULES}
    dirs = {key: [] for key, _ in RULES}
    keep = runtime_entry_keeper(root)
    keep_ts = os.environ.get("DSH_RUNTIME_KEEP_TS") == "1"
    keep_headers = os.environ.get("DSH_RUNTIME_KEEP_NODE_HEADERS") == "1"
    node_dir = os.path.join(root, "node")

    for path in walk_files(root):
        name = os.path.basename(path)
        lower = path.lower()
        if name.endswith(".map"):
            files["R8"].append(path)
        elif name.endswith(".pdb"):
            files["R3"].append(path)
        elif name.endswith(TYPE_SUFFIXES):
            if not keep_ts and path not in keep:
                files["R1"].append(path)
        elif name.endswith(SOURCE_SUFFIXES):
            if not keep_ts and path not in keep:
                files["R2"].append(path)

    for dirpath, dirnames, _ in os.walk(root):
        for name in list(dirnames):
            path = os.path.join(dirpath, name)
            if os.path.islink(path):
                continue
            lower = dirpath.lower()
            if os.path.basename(dirpath) == "prebuilds" and foreign_payload(name, target_platform, target_arch):
                dirs["R4"].append(path)
                dirnames.remove(name)
            elif os.path.basename(dirpath) == "bin" and os.path.basename(os.path.dirname(dirpath)) == "native" \
                    and foreign_payload(name, target_platform, target_arch):
                dirs["R4"].append(path)
                dirnames.remove(name)
            elif name == "conpty" and target_platform != "win32" and "/node-pty/" in lower:
                dirs["R4"].append(path)
                dirnames.remove(name)
            elif name == "libreoffice-kit-wasm" and target_platform in ("darwin", "win32"):
                dirs["R7"].append(path)
                dirnames.remove(name)
            elif os.path.basename(dirpath) == "@pnpm" and (name.startswith("exe.") or name.startswith("exe-")):
                dirs["R6"].append(path)
                dirnames.remove(name)
            elif name == "corepack" and os.path.dirname(path) in (
                os.path.join(node_dir, "lib", "node_modules"),
                os.path.join(node_dir, "bin"),
            ):
                dirs["R5"].append(path)
                dirnames.remove(name)

    for sub in ("include", "share/doc", "share/man"):
        path = os.path.join(node_dir, sub)
        if sub == "include" and keep_headers:
            continue
        if os.path.isdir(path):
            dirs["R5"].append(path)

    corepack_link = os.path.join(node_dir, "bin", "corepack")
    if os.path.islink(corepack_link):
        files["R5"].append(corepack_link)

    # R6 only when the launcher it duplicates is still there: on an unknown layout
    # (notably Windows, where the entry may be a .cmd shim), deleting @pnpm/exe-* could
    # remove the only pnpm binary. A small JS shim therefore keeps the payload.
    pnpm_root = os.path.join(root, "tools", "lib", "node_modules", "pnpm")
    launchers = [os.path.join(pnpm_root, name) for name in ("pnpm", "pnpm.exe", "bin", "pnpm.cjs")]
    has_launcher = any(os.path.isfile(p) and not os.path.islink(p) and os.path.getsize(p) > 1048576 for p in launchers)
    if not has_launcher:
        files["R6"] = []
        dirs["R6"] = []

    return files, dirs


def remove(files, dirs):
    counts = {key: 0 for key, _ in RULES}
    sizes = {key: 0 for key, _ in RULES}
    for key, _ in RULES:
        for path in files[key]:
            try:
                sizes[key] += os.lstat(path).st_size
                os.unlink(path)
                counts[key] += 1
            except OSError as error:
                print("  [skip] {}: {}".format(path, error), file=sys.stderr)
        for path in dirs[key]:
            total = 0
            count = 0
            for dirpath, _, filenames in os.walk(path):
                for name in filenames:
                    candidate = os.path.join(dirpath, name)
                    try:
                        total += os.lstat(candidate).st_size
                        count += 1
                    except OSError:
                        pass
            try:
                shutil.rmtree(path)
                sizes[key] += total
                counts[key] += count
            except OSError as error:
                print("  [skip] {}: {}".format(path, error), file=sys.stderr)
    return counts, sizes


def main():
    parser = argparse.ArgumentParser(description="Remove payload the bundled runtime never loads.")
    parser.add_argument("runtime")
    parser.add_argument("--platform", default=host_platform_name(), help="target platform (darwin/win32/linux)")
    parser.add_argument("--arch", default=normalize_arch(host_platform.machine()), help="target architecture")
    args = parser.parse_args()

    root = os.path.abspath(args.runtime)
    target_platform = normalize_platform(args.platform)
    target_arch = normalize_arch(args.arch)

    node = os.path.join(root, "node")
    node_bin = [os.path.join(node, "bin", "node"), os.path.join(node, "node.exe")]
    dsh = os.path.join(root, "dsh-prefix")
    if not any(os.path.isfile(p) for p in node_bin) or not os.path.isdir(dsh):
        print("{} does not look like a staging tree (no node or dsh-prefix); refusing".format(root), file=sys.stderr)
        return 2

    files, dirs = collect(root, target_platform, target_arch)
    counts, sizes = remove(files, dirs)

    total_count = 0
    total_size = 0
    print("prune {} (target {}-{}):".format(root, target_platform, target_arch))
    for key, title in RULES:
        total_count += counts[key]
        total_size += sizes[key]
        print("  {} {:<26} {:>7} files {:>10}".format(key, title, counts[key], human(sizes[key])))
    print("  {:<29} {:>7} files {:>10}".format("total", total_count, human(total_size)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
