#!/bin/sh
set -e

symbols=$(readelf --wide --symbols "$1")
printf '%s\n' "$symbols" | awk '
    # PowerPC local-entry annotations can add fields before the index and name.
    $5 ~ /^(GLOBAL|WEAK)$/ && $6 == "DEFAULT" && $(NF-1) != "UND" &&
    $NF !~ /^(aligned_alloc|malloc_usable_size|__x86\.get_pc_thunk\.[a-z]+)$/ {
        print "unexpected public allocator symbol " $NF
        bad = 1
    }
    END { exit bad }
'
