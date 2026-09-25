#!/usr/bin/env python3
"""Summarize a samply profile recorded with `--unstable-presymbolicate`.

    python scripts/samply_top.py profile.json.gz [--process dhx] [--thread NAME] [--top N]

Prints the hottest functions by self and inclusive CPU time. `just profile`
records a profile in this shape.

samply's presymbolication can miss the Rust symbols in dhx's PDB and name
them `fun_<rva>`. Those are resolved with llvm-symbolizer (LLVM) in one batch
when it is installed; pass `--symbolizer ""` to skip that.
"""

import argparse
import bisect
import collections
import gzip
import json
import os
import re
import shutil
import struct
import subprocess
import sys

DEFAULT_SYMBOLIZER = shutil.which("llvm-symbolizer") or r"C:\Program Files\LLVM\bin\llvm-symbolizer.exe"
UNNAMED = re.compile(r"^fun_([0-9a-f]+)$")


def load(path):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt", encoding="utf-8") as f:
        profile = json.load(f)
    syms_path = path[:-3] if path.endswith(".gz") else path
    with open(syms_path + ".syms.json", encoding="utf-8") as f:
        syms = json.load(f)
    return profile, syms


def symbolizer(syms):
    names = syms["string_table"]
    tables = {}
    for lib in syms["data"]:
        table = sorted((s["rva"], s["rva"] + s["size"], names[s["symbol"]]) for s in lib["symbol_table"])
        tables[lib["debug_name"].lower()] = ([start for start, _, _ in table], table)

    def symbol(lib, rva):
        found = tables.get(lib["debugName"].lower())
        if found:
            starts, table = found
            i = bisect.bisect_right(starts, rva) - 1
            if i >= 0 and rva < table[i][1]:
                return table[i][2]
        return f"{lib['name']}+{rva:#x}"

    return symbol


def thread_samples(thread, libs, symbol, wall, unnamed):
    frames, funcs, resources = thread["frameTable"], thread["funcTable"], thread["resourceTable"]
    stack_prefix, stack_frame = thread["stackTable"]["prefix"], thread["stackTable"]["frame"]
    samples = thread["samples"]
    # CPU time per sample by default, so threads parked in a wait don't count.
    cpu = samples.get("threadCPUDelta")
    use_cpu = bool(cpu) and not wall
    weights = cpu if use_cpu else (samples.get("weight") or [1] * samples["length"])
    frame_names = {}

    def frame_name(frame):
        if frame not in frame_names:
            lib_index = resources["lib"][funcs["resource"][frames["func"][frame]]]
            if lib_index is None:
                name = thread["stringArray"][funcs["name"][frames["func"][frame]]]
            else:
                lib = libs[lib_index]
                name = symbol(lib, frames["address"][frame])
                if UNNAMED.match(name):
                    unnamed[lib["path"]].add(name)
            frame_names[frame] = name
        return frame_names[frame]

    for stack, weight in zip(samples["stack"], weights):
        chain = []
        while stack is not None:
            chain.append(frame_name(stack_frame[stack]))
            stack = stack_prefix[stack]
        yield chain, weight or (0 if use_cpu else 1)


def image_base(image):
    with open(image, "rb") as f:
        header = f.read(4096)
    pe = struct.unpack_from("<I", header, 0x3C)[0]
    optional = pe + 24
    if struct.unpack_from("<H", header, optional)[0] == 0x20B:  # PE32+
        return struct.unpack_from("<Q", header, optional + 24)[0]
    return struct.unpack_from("<I", header, optional + 28)[0]


def resolve_unnamed(symbolizer, image, names):
    """Map `fun_<rva>` names to real symbols with one llvm-symbolizer run."""
    ordered = sorted(names)
    base = image_base(image)
    stdin = "".join(f"{base + int(UNNAMED.match(name)[1], 16):#x}\n" for name in ordered)
    out = subprocess.run(
        [symbolizer, f"--obj={image}", "--no-inlines", "--output-style=GNU"],
        input=stdin, capture_output=True, text=True, errors="replace", timeout=600,
    ).stdout
    # Two lines per address: the function, then its source location.
    functions = out.splitlines()[::2]
    return {
        name: re.sub(r" \(\.llvm\.\d+\)$", "", function)
        for name, function in zip(ordered, functions)
        if function and function != "??"
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("profile")
    parser.add_argument("--process", help="substring of the process name to keep, e.g. dhx")
    parser.add_argument("--thread", help="substring of the thread name to keep")
    parser.add_argument("--top", type=int, default=25)
    parser.add_argument("--wall", action="store_true", help="weight by samples, not CPU time")
    parser.add_argument(
        "--symbolizer", default=DEFAULT_SYMBOLIZER, help="llvm-symbolizer used to name `fun_` frames"
    )
    args = parser.parse_args()

    profile, syms = load(args.profile)
    symbol = symbolizer(syms)
    unnamed = collections.defaultdict(set)
    samples = []
    for thread in profile["threads"]:
        if args.thread and args.thread not in thread.get("name", ""):
            continue
        if args.process and args.process not in thread.get("processName", ""):
            continue
        samples.extend(thread_samples(thread, profile["libs"], symbol, args.wall, unnamed))

    rename = {}
    if args.symbolizer and os.path.exists(args.symbolizer):
        for image, names in unnamed.items():
            if os.path.exists(image):
                rename.update(resolve_unnamed(args.symbolizer, image, names))

    self_time, total_time = collections.Counter(), collections.Counter()
    total = 0
    for chain, weight in samples:
        if not chain or not weight:
            continue
        chain = [rename.get(name, name) for name in chain]
        total += weight
        self_time[chain[0]] += weight
        for name in set(chain):
            total_time[name] += weight

    if not total:
        sys.exit("no samples matched")
    unit = "samples" if args.wall else "cpu units"
    for title, counter in (("self", self_time), ("inclusive", total_time)):
        print(f"== top {args.top} by {title} ({total} {unit})")
        for name, count in counter.most_common(args.top):
            print(f"{100 * count / total:6.1f}%  {count:9}  {name}")


if __name__ == "__main__":
    main()
