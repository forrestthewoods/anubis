#!/bin/bash
# Outputs don't depend on where the workspace is (#272): builds the same
# targets in two copies of the workspace at different paths and checks that
# every object, library, executable, shared library, PDB and dSYM is
# byte-identical, and that none of them contains either workspace's path.
# Then the second copy rebuilds from the first copy's cache (every action a
# hit) and is compared again.
# Linux host (it has every toolchain). Run from the repo root after
# install-toolchains: tools/relocation_test.sh [anubis binary]
set -u
ANUBIS=$(realpath "${1:-./target/release/anubis}")
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
A="$TMP/a/ws"
B="$TMP/somewhere/much/longer/second ws"
TARGETS=(//samples/basic/trivial_c:main //samples/basic/trivial_cpp:main //samples/basic/shared_cpp:main)
X64_TARGETS=(//samples/basic/nasm_c:main //samples/basic/pragma_lib:main)  # NASM is x86 only; pragma_lib is Windows

build() { # workspace, extra anubis flags...
  local ws=$1; shift
  (
    cd "$ws" &&
    "$ANUBIS" build --no-tui "$@" -m //mode:linux_dev -t "${TARGETS[@]}" //samples/basic/nasm_c:main &&
    "$ANUBIS" build --no-tui "$@" -m //mode:linux_release_static -t //samples/basic/trivial_cpp:main &&
    "$ANUBIS" build --no-tui "$@" -m //mode:win_dev -t "${TARGETS[@]}" "${X64_TARGETS[@]}" &&
    "$ANUBIS" build --no-tui "$@" -m //mode:macos_dev -t "${TARGETS[@]}"
  ) > "$TMP/build.log" 2>&1 || { echo "FAIL build in $ws"; tail -20 "$TMP/build.log"; exit 1; }
}

for ws in "$A" "$B"; do
  mkdir -p "$ws"
  git ls-files -z | xargs -0 cp --parents -t "$ws"
  cp -a toolchains/. "$ws/toolchains/"  # symlinks into the Natron store
done
# A result isn't cached if an input changed at or after its action started
# (#191): don't let the copies' fresh timestamps race A's first compiles.
sleep 1
build "$A"             # fills A's cache
build "$B" --no-cache

# Every build output. Under zig_glibc/.../zig/ is Zig's own cache: its
# bookkeeping names paths, and the objects from it that a link uses are
# compared as part of every executable and shared library.
outputs() {
  (cd "$1/.anubis-out" && find . -type f \( -name '*.o' -o -name '*.obj' -o -name '*.a' -o -name '*.lib' \
    -o -name '*.so' -o -name '*.dll' -o -name '*.dylib' -o -name '*.exe' -o -name '*.pdb' -o -path '*/bin/main' \
    -o -path '*.dSYM/Contents/Resources/DWARF/*' \) ! -path '*/zig/*' | sort)
}
status=0
compare() { # label
  outputs "$A" > "$TMP/a.txt"
  outputs "$B" > "$TMP/b.txt"
  if ! diff "$TMP/a.txt" "$TMP/b.txt"; then echo "FAIL $1: different output lists"; status=1; fi
  local n=0
  while IFS= read -r f; do
    n=$((n+1))
    if ! cmp -s "$A/.anubis-out/$f" "$B/.anubis-out/$f"; then echo "FAIL $1: differs: $f"; status=1; fi
    for ws in "$A" "$B"; do
      if grep -qaF "$ws" "$ws/.anubis-out/$f"; then echo "FAIL $1: $ws/.anubis-out/$f contains its workspace path"; status=1; fi
    done
  done < "$TMP/a.txt"
  [ "$n" -gt 50 ] || { echo "FAIL $1: only $n outputs found"; status=1; }
  echo "$1: compared $n outputs"
}
compare "fresh builds"

# B from A's cache: every action must hit, and give the same bytes.
rm -rf "$B/.anubis-out" "$B/.anubis-cache"
cp -a "$A/.anubis-cache" "$B/.anubis-cache"
build "$B"
if grep -o 'build cache: [0-9]* hits, [0-9]* misses' "$TMP/build.log" | grep -qv ' 0 misses'; then
  echo "FAIL: rebuilding from the other workspace's cache missed"; grep 'build cache:' "$TMP/build.log"; status=1
fi
compare "from the other workspace's cache"

[ $status = 0 ] && echo "PASS"
exit $status
