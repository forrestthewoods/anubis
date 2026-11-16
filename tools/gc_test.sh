#!/bin/bash
# End-to-end test of `anubis gc` (src/gc.rs) on real build outputs: a target
# directory whose stamp is old is deleted, everything else stays, and the
# deleted target comes back from the cache.
# Run from the repo root: tools/gc_test.sh [anubis binary]
set -eu
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) MODE=win_dev; EXE=.exe; ANUBIS=${1:-./target/release/anubis.exe} ;;
  *) MODE=linux_dev; EXE=; ANUBIS=${1:-./target/release/anubis} ;;
esac
export MSYS_NO_PATHCONV=1
TARGETS="//samples/basic/trivial_c:main //samples/basic/trivial_cpp:main"
c_dir() { echo .anubis-out/${MODE}_*/trivial_c_main_*; }
cpp_exe() { echo .anubis-out/${MODE}_*/trivial_cpp_main_*/bin/main$EXE; }
expect() { echo "$1"; echo "$1" | grep -q "$2" || { echo "FAIL: expected \"$2\""; exit 1; }; }

$ANUBIS build --no-tui -m //mode:$MODE -t $TARGETS > /dev/null 2>&1

# Everything was just used: nothing to do, even with a one-day limit.
expect "$($ANUBIS gc --max-age 1)" "removed 0 target directories, 0 cache entries"

# As if trivial_c was last built three days ago.
touch -d '3 days ago' "$(c_dir)/.anubis-used"
expect "$($ANUBIS gc --max-age 1 --dry-run)" "would remove 1 target directories"
test -e "$(c_dir)/bin/main$EXE" || { echo "FAIL: --dry-run deleted something"; exit 1; }

expect "$($ANUBIS gc --max-age 1)" "removed 1 target directories"
test ! -e "$(c_dir)" || { echo "FAIL: stale target directory still there"; exit 1; }
"$(cpp_exe)" > /dev/null || { echo "FAIL: a fresh target was damaged"; exit 1; }

# Its cache entries were used an hour ago at most, so it restores without a miss.
expect "$($ANUBIS build --no-tui -m //mode:$MODE -t $TARGETS 2>&1 | grep 'build cache:')" " 0 misses"
"$(c_dir)/bin/main$EXE" > /dev/null
echo "PASS"
