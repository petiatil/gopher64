#!/usr/bin/env python3
"""Compare the netplay sync logs of two clients.

Each client writes one when started with GOPHER64_NETPLAY_SYNC_LOG=<file>:
a line per checkpoint with the frame number and one hash per 64 KB of the
game's memory. The last line written for a frame is the final one.

    python3 netplay_sync_compare.py mac.log pc.log --ignore 47-56

--ignore lists 64 KB regions that hold frame and depth buffers. The renderer
writes those back whenever the GPU gets to it, so they can differ for a moment
without the game being out of sync. For Mario Kart 64 (USA) they are regions
47-56 (0x802F9F80-0x8038FF80: gZBuffer and the three framebuffers).

Exit status 0 means every compared checkpoint matched.
"""
import argparse
import sys


def final_lines(path):
    frames = {}
    with open(path) as log:
        for line in log:
            parts = line.split()
            if len(parts) > 2:
                frames[int(parts[0])] = parts[1:]
    return frames


def parse_regions(text):
    regions = set()
    for part in filter(None, text.split(",")):
        first, _, last = part.partition("-")
        regions.update(range(int(first), int(last or first) + 1))
    return regions


def main():
    parser = argparse.ArgumentParser(description="Compare the netplay sync logs of two clients.")
    parser.add_argument("first")
    parser.add_argument("second")
    parser.add_argument("--ignore", default="", help="regions to skip, e.g. 47-56")
    args = parser.parse_args()

    ignored = parse_regions(args.ignore)
    first, second = final_lines(args.first), final_lines(args.second)
    # the newest checkpoints may still have been rolled back on one side
    common = sorted(set(first) & set(second))[:-2]
    if not common:
        print("No common checkpoints: the session was too short, or the logs are unrelated.")
        return 2

    differing = {}
    for frame in common:
        regions = [
            index
            for index, (a, b) in enumerate(zip(first[frame], second[frame]))
            if a != b and index not in ignored
        ]
        if len(first[frame]) != len(second[frame]):
            regions.append(-1)
        if regions:
            differing[frame] = regions

    print(f"Checkpoints compared: {len(common)} (frames {common[0]} to {common[-1]})")
    if not differing:
        print("In sync: every checkpoint matched.")
        return 0
    print(f"OUT OF SYNC at {len(differing)} checkpoint(s), first at frame {min(differing)}.")
    for frame in sorted(differing)[:10]:
        print(f"  frame {frame}: regions {differing[frame]}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
