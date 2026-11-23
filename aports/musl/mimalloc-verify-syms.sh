#!/bin/sh
set -e

symbols=$(readelf --wide --symbols "$1")
printf '%s\n' "$symbols" | awk '
    $5 ~ /^(GLOBAL|WEAK)$/ && $6 == "DEFAULT" && $7 != "UND" &&
    $8 !~ /^(aligned_alloc|malloc_usable_size|__x86\.get_pc_thunk\.[a-z]+)$/ {
        print "unexpected public allocator symbol " $8
        bad = 1
    }
    END { exit bad }
'
