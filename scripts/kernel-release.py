#!/usr/bin/env python3
"""Print the kernel release this build would use, so a workflow can pin it."""
import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from kernel_packages import kernels

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--index", action="store_true",
                    help="print the whole resolved index instead of only the tag")
args = parser.parse_args()

index = kernels()
print(json.dumps(index, indent=2) + "\n" if args.index else index["tag"])
