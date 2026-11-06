#!/bin/bash
# End-to-end test of the compile cache's "must not exist" headers
# (src/rules/cc_includes.rs): a header created where the compiler would now
# find it first must be a cache miss, and unrelated new files must not be.
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
run() { # target -> prints "<hits> <misses> <value>"
  local hm; hm=$($ANUBIS build --no-tui -m //mode:$MODE -t $1 2>&1 | grep -o '[0-9]* hits, [0-9]* misses' | tail -1)
  local rel=${1#//}; rel=${rel%:main}
  local out; out=$(.anubis-out/${MODE}_*/$rel/main/bin/main$EXE 2>&1 | tr -d '\r')
  local v; v=$(echo "$out" | grep -o 'VALUE=[0-9A-Za-z_]*' | tail -1)
  echo "$hm $v"
}
check() { # name target expect_value expect(miss|hit)
  local r; r=$(run $2)
  local misses; misses=$(echo "$r" | sed -E 's/.* ([0-9]+) misses.*/\1/')
  local ok=1
  [[ "$r" == *"VALUE=$3"* ]] || ok=0
  if [ "$4" = hit ] && [ "$misses" != 0 ]; then ok=0; fi
  if [ "$4" = miss ] && [ "$misses" = 0 ]; then ok=0; fi
  if [ $ok = 1 ]; then pass=$((pass+1)); echo "PASS $1: $r"; else fail=$((fail+1)); echo "FAIL $1: $r (want VALUE=$3, $4)"; fi
}
bin() { # dir srcs include_dirs
  printf 'cc_binary(\n    name = "main",\n    lang = "c",\n    srcs = [ %s ],\n    include_dirs = [ %s ],\n)\n' "$2" "$3" > $1/ANUBIS
}

# 1. -I shadowing: inc_b/x.h used, then inc_a/x.h appears.
d=$R/shadow; mkdir -p $d/inc_a $d/inc_b
echo '#define V 1' > $d/inc_b/x.h
printf "// $SALT\n"'#include <stdio.h>\n#include "x.h"\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/main.c
bin $d 'RelPath("main.c")' 'RelPath("inc_a"), RelPath("inc_b")'
check shadow-first //$d:main 1 miss
check shadow-warm //$d:main 1 hit
echo '#define V 2' > $d/inc_a/x.h
check shadow-new //$d:main 2 miss
touch $d/inc_a/unrelated.h $d/inc_b/Makefile $d/LICENSE
check unrelated-files //$d:main 2 hit
rm $d/inc_a/x.h
check shadow-removed //$d:main 1 hit

# 2. Quote include from a header's own dir.
d=$R/quote; mkdir -p $d/src $d/other $d/inc
echo '#include "y.h"' > $d/other/o.h
echo '#define V 1' > $d/inc/y.h
printf "// $SALT\n"'#include <stdio.h>\n#include "../other/o.h"\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/src/main.c
bin $d 'RelPath("src/main.c")' 'RelPath("inc")'
check quote-first //$d:main 1 miss
echo '#define V 2' > $d/other/y.h
check quote-includer-dir //$d:main 2 miss

# 3. '..' spelling.
d=$R/dotdot; mkdir -p $d/pkg/src $d/inc/deep
echo '#define V 1' > $d/inc/common.h
printf "// $SALT\n"'#include <stdio.h>\n#include "../common.h"\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/pkg/src/main.c
touch $d/inc/deep/keep.h
bin $d 'RelPath("pkg/src/main.c")' 'RelPath("inc/deep")'
check dotdot-first //$d:main 1 miss
echo '#define V 2' > $d/pkg/common.h
check dotdot-new //$d:main 2 miss

# 4. Subpackage dir next to the source.
d=$R/subpkg; mkdir -p $d/inc/sub $d/sub
echo '#define V 1' > $d/inc/sub/x.h
echo '' > $d/sub/ANUBIS
printf "// $SALT\n"'#include <stdio.h>\n#include "sub/x.h"\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/main.c
bin $d 'RelPath("main.c")' 'RelPath("inc")'
check subpkg-first //$d:main 1 miss
echo '#define V 2' > $d/sub/x.h
check subpkg-new //$d:main 2 miss

# 5. __has_include of a header that doesn't exist yet, in a new subdir.
d=$R/hasinc; mkdir -p $d/inc
printf "// $SALT\n"'#include <stdio.h>\n#if __has_include(<opt/opt.h>)\n#include <opt/opt.h>\n#else\n#define V 1\n#endif\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/main.c
touch $d/inc/keep.h
bin $d 'RelPath("main.c")' 'RelPath("inc")'
check hasinc-first //$d:main 1 miss
mkdir -p $d/inc/opt; echo '#define V 2' > $d/inc/opt/opt.h
check hasinc-new //$d:main 2 miss

# 6. #include_next with a new header in between.
d=$R/next; mkdir -p $d/wrap $d/mid $d/real
echo '#include_next <foo.h>' > $d/wrap/foo.h
echo '#define V 1' > $d/real/foo.h
printf "// $SALT\n"'#include <stdio.h>\n#include <foo.h>\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/main.c
bin $d 'RelPath("main.c")' 'RelPath("wrap"), RelPath("mid"), RelPath("real")'
check next-first //$d:main 1 miss
echo '#define V 2' > $d/mid/foo.h
check next-new //$d:main 2 miss

# 7. A project header shadowing a toolchain header.
d=$R/sys; mkdir -p $d/inc
printf "// $SALT\n"'#include <stdio.h>\n#include <stdlib.h>\n#ifndef V\n#define V 1\n#endif\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/main.c
touch $d/inc/keep.h
bin $d 'RelPath("main.c")' 'RelPath("inc")'
check sys-first //$d:main 1 miss
printf '#include_next <stdlib.h>\n#define V 2\n' > $d/inc/stdlib.h
check sys-shadow //$d:main 2 miss

# 8. A nested header's quote include: GNU mode searches only the includer's
#    dir, never the source's dir (a decoy x.h sits there). MSVC mode (win_dev)
#    searches the source's dir too, so it finds the decoy.
if [ $MODE = win_dev ]; then DECOY=9; NEW=hit; else DECOY=; NEW=miss; fi
d=$R/nested; mkdir -p $d/sub $d/inc_a $d/inc_b
echo '#include "x.h"' > $d/sub/h.h
echo '#define V 9' > $d/x.h
echo '#define V 1' > $d/inc_b/x.h
printf "// $SALT\n"'#include <stdio.h>\n#include "sub/h.h"\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/main.c
bin $d 'RelPath("main.c")' 'RelPath("inc_a"), RelPath("inc_b")'
check nested-first //$d:main ${DECOY:-1} miss
echo '#define V 2' > $d/inc_a/x.h
check nested-new //$d:main ${DECOY:-2} $NEW

# 9. ...and in GNU mode a new file next to the source doesn't recompile.
if [ $MODE = win_dev ]; then NEW=miss; V9=2; else NEW=hit; V9=1; fi
d=$R/srcdir; mkdir -p $d/inc $d/inc2
echo '#include "y.h"' > $d/inc/h.h
echo '#define V 1' > $d/inc2/y.h
printf "// $SALT\n"'#include <stdio.h>\n#include <h.h>\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/main.c
bin $d 'RelPath("main.c")' 'RelPath("inc"), RelPath("inc2")'
check srcdir-first //$d:main 1 miss
echo '#define V 2' > $d/y.h
check srcdir-new //$d:main $V9 $NEW

# 10. #include_next from a header found in a nested -I dir.
d=$R/nestnext; mkdir -p $d/inc/sub $d/mid $d/real
echo '#include_next <x.h>' > $d/inc/sub/x.h
echo '#define V 1' > $d/real/x.h
printf "// $SALT\n"'#include <stdio.h>\n#include <x.h>\nint main(void){printf("VALUE=%%d\\n", V);return 0;}\n' > $d/main.c
bin $d 'RelPath("main.c")' 'RelPath("inc"), RelPath("inc/sub"), RelPath("mid"), RelPath("real")'
check nestnext-first //$d:main 1 miss
echo '#define V 2' > $d/mid/x.h
check nestnext-new //$d:main 2 miss

# 11. Names in comments and #if 0 aren't lookups.
d=$R/comment; mkdir -p $d/inc
printf "// $SALT\n"'#include <stdio.h>\n// #include "c1.h"\n/* #include "c2.h" */\n#if 0\n#include "c3.h"\n#endif\nint main(void){printf("VALUE=%%d\\n", 1);return 0;}\n' > $d/main.c
touch $d/inc/keep.h
bin $d 'RelPath("main.c")' 'RelPath("inc")'
check comment-first //$d:main 1 miss
touch $d/c1.h $d/c2.h $d/c3.h $d/inc/c1.h
check comment-new //$d:main 1 hit

echo "passed $pass, failed $fail"
[ $fail = 0 ]
