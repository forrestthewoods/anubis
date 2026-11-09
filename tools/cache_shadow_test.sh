#!/bin/bash
# End-to-end test of declared headers (src/rules/declared_headers.rs):
# - adding, removing or renaming a declared header is a cache miss for the
#   compiles that can see it (shadowing, __has_include), and reverting is a hit;
# - files outside the header lists don't affect the cache;
# - a compile that reads an undeclared header, or a file outside the
#   workspace, fails.
# Run from the repo root: tools/cache_shadow_test.sh [anubis binary]
set -u
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) MODE=win_dev; EXE=.exe; ANUBIS=${1:-./target/release/anubis.exe} ;;
  *) MODE=linux_dev; EXE=; ANUBIS=${1:-./target/release/anubis} ;;
esac
export MSYS_NO_PATHCONV=1
R=samples/tmp_cache_shadow_test
trap 'rm -rf $R' EXIT
rm -rf $R; mkdir -p $R
SALT=$RANDOM$RANDOM  # fresh sources, so earlier runs' cache entries don't match
pass=0; fail=0
LOG=$R/last.log
run() { # target -> prints "<hits> <misses> <value>"
  local rel=${1#//}; rel=${rel%:main}
  rm -f .anubis-out/${MODE}_*/$rel/main/bin/main$EXE  # never read a previous build's binary
  if ! $ANUBIS build --no-tui -m //mode:$MODE -t $1 > $LOG 2>&1; then
    echo "BUILD_FAILED"; tail -5 $LOG >&2; return
  fi
  local hm; hm=$(grep -o '[0-9]* hits, [0-9]* misses' $LOG | tail -1)
  local out; out=$(.anubis-out/${MODE}_*/$rel/main/bin/main$EXE 2>&1 | tr -d '\r')
  local v; v=$(echo "$out" | grep -o 'VALUE=[0-9A-Za-z_]*' | tail -1)
  echo "$hm $v"
}
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; }
check() { # name target expect_value expect(miss|hit)
  local r; r=$(run $2)
  local misses; misses=$(echo "$r" | sed -nE 's/.* ([0-9]+) misses.*/\1/p')
  local good=1
  [ -n "$misses" ] || good=0  # build failed or printed no cache stats
  [[ "$r" == *"VALUE=$3"* ]] || good=0
  if [ "$4" = hit ] && [ "$misses" != 0 ]; then good=0; fi
  if [ "$4" = miss ] && [ "$misses" = 0 ]; then good=0; fi
  if [ $good = 1 ]; then ok "$1: $r"; else bad "$1: $r (want VALUE=$3, $4)"; fi
}
check_fails() { # name target expected-text: a fresh, uncached build must fail with it
  local rel=${2#//}; rel=${rel%:main}
  rm -rf .anubis-out/${MODE}_*/$rel  # only this test's outputs: CI runs the samples afterwards
  if $ANUBIS build --no-tui --no-cache -m //mode:$MODE -t $2 > $LOG 2>&1; then
    bad "$1: build succeeded"
  elif grep -q "$3" $LOG; then
    ok "$1: fails with \"$3\""
  else
    bad "$1: failed without \"$3\""; tail -5 $LOG
  fi
}
bin() { # dir srcs include_dirs headers
  printf 'cc_binary(\n    name = "main",\n    lang = "c",\n    srcs = [ %s ],\n    include_dirs = [ %s ],\n    headers = %s,\n)\n' "$2" "$3" "$4" > $1/ANUBIS
}
main_c() { # file include-line(s)
  printf "// $SALT\n"'#include <stdio.h>\n'"$2"'\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $1
}

# 1. -I shadowing with globbed header lists: a new declared header is a miss.
d=$R/shadow; mkdir -p $d/inc_a $d/inc_b
echo '#define V 1' > $d/inc_b/x.h
main_c $d/main.c '#include "x.h"'
bin $d 'RelPath("main.c")' 'RelPath("inc_a"), RelPath("inc_b")' 'glob([ "inc_a/**/*", "inc_b/**/*" ])'
check shadow-first //$d:main 1 miss
check shadow-warm //$d:main 1 hit
echo '#define V 2' > $d/inc_a/x.h
check shadow-new //$d:main 2 miss
touch $d/LICENSE $d/Makefile $d/notes.txt
check undeclared-files-ignored //$d:main 2 hit
echo '#define W 3' > $d/inc_b/unused.h
check new-declared-header-misses //$d:main 2 miss
echo '#define W 4' > $d/inc_b/unused.h
check edit-unread-header-hits //$d:main 2 hit
rm $d/inc_a/x.h $d/inc_b/unused.h
check revert-hits //$d:main 1 hit

# 2. __has_include of a header declared later.
d=$R/hasinc; mkdir -p $d/inc
touch $d/inc/keep.h
main_c $d/main.c '#if __has_include(<opt/opt.h>)\n#include <opt/opt.h>\n#else\n#define V 1\n#endif'
bin $d 'RelPath("main.c")' 'RelPath("inc")' 'glob([ "inc/**/*" ])'
check hasinc-first //$d:main 1 miss
mkdir -p $d/inc/opt; echo '#define V 2' > $d/inc/opt/opt.h
check hasinc-new //$d:main 2 miss

# 3. Quote include from a header's own dir, '..' spellings.
d=$R/quote; mkdir -p $d/src $d/other $d/inc
echo '#include "y.h"' > $d/other/o.h
echo '#define V 1' > $d/inc/y.h
main_c $d/src/main.c '#include "../other/o.h"'
bin $d 'RelPath("src/main.c")' 'RelPath("inc")' 'glob([ "inc/**/*", "other/**/*" ])'
check quote-first //$d:main 1 miss
echo '#define V 2' > $d/other/y.h
check quote-includer-dir //$d:main 2 miss

# 4. Reading an undeclared header fails the build.
d=$R/undeclared; mkdir -p $d/inc
echo '#define V 1' > $d/inc/x.h
echo '#define V 1' > $d/inc/y.h
main_c $d/main.c '#include "y.h"'
bin $d 'RelPath("main.c")' 'RelPath("inc")' 'RelPaths([ "inc/x.h" ])'
check_fails undeclared-header //$d:main "inc/y.h"

# 5. An undeclared header that would shadow: a fresh build fails the check.
d=$R/undeclared_shadow; mkdir -p $d/inc_a $d/inc_b
echo '#define V 1' > $d/inc_b/x.h
main_c $d/main.c '#include "x.h"'
bin $d 'RelPath("main.c")' 'RelPath("inc_a"), RelPath("inc_b")' 'RelPaths([ "inc_b/x.h" ])'
check undeclared-shadow-first //$d:main 1 miss
echo '#define V 2' > $d/inc_a/x.h
check_fails undeclared-shadow //$d:main "inc_a/x.h"
rm $d/inc_a/x.h

# 6. A header that isn't in any list a dependency exports.
d=$R/layering; mkdir -p $d/lib $d/priv
echo '#define V 1' > $d/lib/pub.h
echo '#define V 1' > $d/priv/secret.h
main_c $d/main.c '#include "secret.h"'
printf 'cc_binary(\n    name = "main",\n    lang = "c",\n    srcs = [ RelPath("main.c") ],\n    deps = Targets([ ":lib" ]),\n)\n\ncc_static_library(\n    name = "lib",\n    lang = "c",\n    srcs = [],\n    public_include_dirs = [ RelPath("lib"), RelPath("priv") ],\n    public_headers = [ RelPath("lib/pub.h") ],\n)\n' > $d/ANUBIS
check_fails layering //$d:main "priv/secret.h"

# 7. Files outside the workspace fail, even through an absolute include.
if [ $MODE = linux_dev ]; then
  OUTSIDE=$(mktemp -d); echo '#define V 1' > $OUTSIDE/outside.h
  d=$R/outside; mkdir -p $d
  main_c $d/main.c "#include \"$OUTSIDE/outside.h\""
  printf 'cc_binary(\n    name = "main",\n    lang = "c",\n    srcs = [ RelPath("main.c") ],\n)\n' > $d/ANUBIS
  check_fails outside-workspace //$d:main "Outside the workspace"
  rm -rf $OUTSIDE
fi

# 8. A declared header that doesn't exist yet: creating it is a miss.
d=$R/declared_missing; mkdir -p $d/inc_a $d/inc_b
echo '#define V 1' > $d/inc_b/x.h
main_c $d/main.c '#include "x.h"'
bin $d 'RelPath("main.c")' 'RelPath("inc_a"), RelPath("inc_b")' 'RelPaths([ "inc_a/x.h", "inc_b/x.h" ])'
check declared-missing-first //$d:main 1 miss
echo '#define V 2' > $d/inc_a/x.h
check declared-missing-created //$d:main 2 miss

# 9. '..' through a symlink: the cache hashes the file the compiler read,
#    not the lexically folded path.
if [ $MODE = linux_dev ]; then
  OUTSIDE=$(mktemp -d); mkdir -p $OUTSIDE/sub
  echo '#include "../b.h"' > $OUTSIDE/sub/a.h
  echo '#define V 1' > $OUTSIDE/b.h
  d=$R/symlink_dotdot; mkdir -p $d/sym
  ln -s $OUTSIDE/sub $d/sym/ext
  echo '#define V 99' > $d/sym/b.h  # what 'sym/ext/../b.h' folds to
  main_c $d/main.c '#include "sym/ext/a.h"'
  bin $d 'RelPath("main.c")' 'RelPath(".")' 'RelPaths([ "sym/ext/a.h", "sym/b.h" ])'
  check symlink-dotdot-first //$d:main 1 miss
  echo '#define V 2' > $OUTSIDE/b.h
  check symlink-dotdot-edit //$d:main 2 miss
  rm -rf $OUTSIDE
fi

# 10. Compiles run in the workspace root: a root-relative -include works
#     wherever anubis is started.
d=$R/relative_include; mkdir -p $d
echo '#define V 1' > $d/pre.h
main_c $d/main.c ''
printf 'cc_binary(\n    name = "main",\n    lang = "c",\n    srcs = [ RelPath("main.c") ],\n    compiler_flags = [ "-include", "%s/pre.h" ],\n    headers = [ RelPath("pre.h") ],\n)\n' "$d" > $d/ANUBIS
ABS_ANUBIS=$(cd "$(dirname "$ANUBIS")" && pwd)/$(basename "$ANUBIS")
if (cd $d && $ABS_ANUBIS build --no-tui -m //mode:$MODE -t //$d:main > /dev/null 2>&1); then
  ok "relative-include-from-subdir"
else
  bad "relative-include-from-subdir: build failed when started from $d"
fi

# 11. A header list is required with include dirs.
d=$R/required; mkdir -p $d/inc
main_c $d/main.c '#define V 1'
printf 'cc_binary(\n    name = "main",\n    lang = "c",\n    srcs = [ RelPath("main.c") ],\n    include_dirs = [ RelPath("inc") ],\n)\n' > $d/ANUBIS
check_fails headers-required //$d:main "headers. is missing"

echo "passed $pass, failed $fail"
[ $fail = 0 ]
