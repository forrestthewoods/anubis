#!/bin/bash
# Outputs don't depend on where the workspace is (#272): builds the same
# targets in two copies of the workspace at different paths and checks that
# every object, library, executable, shared library and PDB is byte-identical,
# and that none of them contains either workspace's path.
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
status=0
for ws in "$A" "$B"; do
  mkdir -p "$ws"
  git ls-files -z | xargs -0 cp --parents -t "$ws"
  cp -a toolchains/. "$ws/toolchains/"  # symlinks into the Natron store
  (
    cd "$ws" &&
    "$ANUBIS" build --no-tui --no-cache -m //mode:linux_dev -t "${TARGETS[@]}" //samples/basic/nasm_c:main &&
    "$ANUBIS" build --no-tui --no-cache -m //mode:win_dev -t "${TARGETS[@]}" "${X64_TARGETS[@]}" &&
    "$ANUBIS" build --no-tui --no-cache -m //mode:macos_dev -t "${TARGETS[@]}"
  ) > "$TMP/build.log" 2>&1 || { echo "FAIL build in $ws"; tail -20 "$TMP/build.log"; exit 1; }
done

# Every build output; Zig's own cache directory (zig_glibc/.../zig/) holds
# its bookkeeping, not outputs.
outputs() {
  (cd "$1/.anubis-out" && find . -type f \( -name '*.o' -o -name '*.obj' -o -name '*.a' -o -name '*.lib' \
    -o -name '*.so' -o -name '*.dll' -o -name '*.dylib' -o -name '*.exe' -o -name '*.pdb' -o -path '*/bin/main' \) \
    ! -path '*/zig/*' | sort)
}
outputs "$A" > "$TMP/a.txt"
outputs "$B" > "$TMP/b.txt"
if ! diff "$TMP/a.txt" "$TMP/b.txt"; then echo "FAIL different output lists"; status=1; fi
n=0
while IFS= read -r f; do
  n=$((n+1))
  if ! cmp -s "$A/.anubis-out/$f" "$B/.anubis-out/$f"; then echo "FAIL differs: $f"; status=1; fi
  for ws in "$A" "$B"; do
    if grep -qaF "$ws" "$ws/.anubis-out/$f"; then echo "FAIL $ws/.anubis-out/$f contains its workspace path"; status=1; fi
  done
done < "$TMP/a.txt"
[ "$n" -gt 50 ] || { echo "FAIL only $n outputs found"; status=1; }
[ $status = 0 ] && echo "PASS $n outputs identical in both workspaces"
exit $status
