#!/usr/bin/env python3
"""Summarize a samply profile recorded with `--unstable-presymbolicate`.

    python scripts/samply_top.py profile.json.gz [--thread NAME] [--top N]

Prints the hottest functions by self and inclusive sample count, per thread
or across every thread of the profiled process. `just profile` records a
profile in this shape.
"""

import argparse
import bisect
import collections
import gzip
import json
import sys


def load(path):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt", encoding="utf-8") as f:
        profile = json.load(f)
    syms_path = path[:-3] if path.endswith(".gz") else path
    with open(syms_path + ".syms.json", encoding="utf-8") as f:
        syms = json.load(f)
    return profile, syms


def symbolizer(profile, syms):
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


def thread_samples(thread, libs, symbol, wall):
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
                frame_names[frame] = thread["stringArray"][funcs["name"][frames["func"][frame]]]
            else:
                frame_names[frame] = symbol(libs[lib_index], frames["address"][frame])
        return frame_names[frame]

    for stack, weight in zip(samples["stack"], weights):
        chain = []
        while stack is not None:
            chain.append(frame_name(stack_frame[stack]))
            stack = stack_prefix[stack]
        yield chain, weight or (0 if use_cpu else 1)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("profile")
    parser.add_argument("--thread", help="substring of the thread name to keep")
    parser.add_argument("--top", type=int, default=25)
    parser.add_argument("--wall", action="store_true", help="weight by samples, not CPU time")
    args = parser.parse_args()

    profile, syms = load(args.profile)
    symbol = symbolizer(profile, syms)
    self_time, total_time = collections.Counter(), collections.Counter()
    total = 0
    for thread in profile["threads"]:
        if args.thread and args.thread not in thread.get("name", ""):
            continue
        for chain, weight in thread_samples(thread, profile["libs"], symbol, args.wall):
            if not chain or not weight:
                continue
            total += weight
            self_time[chain[0]] += weight
            for name in set(chain):
                total_time[name] += weight

    if not total:
        sys.exit("no samples matched")
    for title, counter in (("self", self_time), ("inclusive", total_time)):
        unit = "samples" if args.wall else "cpu units"
        print(f"== top {args.top} by {title} ({total} {unit})")
        for name, count in counter.most_common(args.top):
            print(f"{100 * count / total:6.1f}%  {count:7}  {name}")


if __name__ == "__main__":
    main()
