#!/bin/bash
# Outputs don't depend on where the workspace is (#272): builds the same
# targets in two copies of the workspace at different paths and checks that
# every object, library, executable, shared library and PDB is byte-identical
# and contains neither workspace's path.
# Linux host (it has every toolchain). Run from the repo root after
# install-toolchains: tools/relocation_test.sh [anubis binary]
set -u
ANUBIS=$(realpath "${1:-./target/release/anubis}")
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
A="$TMP/a/ws"
B="$TMP/somewhere/much/longer/second ws"
T=(//samples/basic/trivial_c:main //samples/basic/trivial_cpp:main //samples/basic/shared_cpp:main)
for ws in "$A" "$B"; do
  mkdir -p "$ws"
  git ls-files -z | xargs -0 cp --parents -t "$ws"
  cp -a toolchains/. "$ws/toolchains/"  # symlinks into the Natron store
  (
    cd "$ws" &&
    "$ANUBIS" build --no-tui --no-cache -m //mode:linux_dev -t "${T[@]}" //samples/basic/nasm_c:main &&
    "$ANUBIS" build --no-tui --no-cache -m //mode:win_dev -t "${T[@]}" //samples/basic/nasm_c:main //samples/basic/pragma_lib:main &&
    "$ANUBIS" build --no-tui --no-cache -m //mode:macos_dev -t "${T[@]}"
  ) > "$TMP/build.log" 2>&1 || { echo "FAIL build in $ws"; tail -20 "$TMP/build.log"; exit 1; }
done

# Zig's own cache (zig_glibc/.../zig/) is left out: what a link uses from it
# is compared as part of every executable.
cd "$A/.anubis-out"
find . -type f \( -name '*.o' -o -name '*.obj' -o -name '*.a' -o -name '*.lib' -o -name '*.so' -o -name '*.dll' \
  -o -name '*.dylib' -o -name '*.exe' -o -name '*.pdb' -o -path '*/bin/main' \) ! -path '*/zig/*' | sort > "$TMP/files"
status=0
while IFS= read -r f; do
  cmp -s "$f" "$B/.anubis-out/$f" || { echo "FAIL differs: $f"; status=1; }
  if grep -qaF -e "$A" -e "$B" "$f" "$B/.anubis-out/$f"; then echo "FAIL contains a workspace path: $f"; status=1; fi
done < "$TMP/files"
n=$(wc -l < "$TMP/files")
[ "$n" -gt 50 ] || { echo "FAIL only $n outputs found"; status=1; }
[ $status = 0 ] && echo "PASS $n outputs identical in both workspaces"
exit $status
