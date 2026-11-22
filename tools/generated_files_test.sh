#!/bin/bash
# End-to-end test of generated files (#213): an anubis_cmd writes gen.c and
# gen.h, a deploy_into_workspace copies them into gen/ (never there before
# the build), one library compiles gen.c and another reads gen.h through
# the first's include dir. Every compile must wait for the deploy, and a
# change to the generator's input must reach the binary.
# Builds two modes at once: the generator and the deploy build for the host
# (Rule::builds_for_host), so both modes share one command job and one deploy.
# Run from the repo root: tools/generated_files_test.sh [anubis binary]
set -u
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) MODE=win_dev; MODE2=win_release; EXE=.exe; ANUBIS=${1:-./target/release/anubis.exe} ;;
  Darwin) MODE=macos_dev; MODE2=macos_release; EXE=; ANUBIS=${1:-./target/release/anubis} ;;
  *) MODE=linux_dev; MODE2=linux_release; EXE=; ANUBIS=${1:-./target/release/anubis} ;;
esac
export MSYS_NO_PATHCONV=1
R=samples/tmp_generated_files_test
T=//$R:main
trap 'rm -rf $R' EXIT
rm -rf $R; mkdir -p $R
pass=0; fail=0
LOG=$R/last.log
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; }

cat > $R/ANUBIS <<'ANUBIS'
cc_binary(
    name = "gen_tool",
    lang = "c",
    srcs = [ RelPath("gen_tool.c") ],
)

anubis_cmd(
    name = "generate",
    tool = Target(":gen_tool"),
    args = [
        [ "header", RelPath("value.txt"), "$OUTDIR/gen.h" ],
        [ "source", RelPath("value.txt"), "$OUTDIR/gen.c" ],
    ],
)

deploy_into_workspace(
    name = "deploy",
    dep = Target(":generate"),
    dst = RelPath("gen"),
    method = "copy",
)

cc_static_library(
    name = "gen",
    lang = "c",
    srcs = [ RelPath("gen/gen.c") ],
    public_include_dirs = [ RelPath("gen") ],
    public_headers = [ RelPath("gen/gen.h") ],
    deps = Targets([ ":deploy" ]),
)

cc_static_library(
    name = "user",
    lang = "c",
    srcs = [ RelPath("user.c") ],
    deps = Targets([ ":gen" ]),
)

cc_binary(
    name = "main",
    lang = "c",
    srcs = [ RelPath("main.c") ],
    deps = Targets([ ":user" ]),
)

# Names a generated file the deploy never writes.
cc_binary(
    name = "missing",
    lang = "c",
    srcs = [ RelPath("main.c"), RelPath("gen/not_generated.c") ],
    deps = Targets([ ":user" ]),
)
ANUBIS

cat > $R/gen_tool.c <<'C'
// gen_tool <header|source> <value file> <output>
#include <stdio.h>
#include <string.h>
int main(int argc, char** argv) {
    int value = 0;
    FILE* in = argc == 4 ? fopen(argv[2], "r") : NULL;
    if (!in || fscanf(in, "%d", &value) != 1) { fprintf(stderr, "gen_tool: bad input\n"); return 1; }
    fclose(in);
    FILE* out = fopen(argv[3], "w");
    if (!out) { fprintf(stderr, "gen_tool: can't write %s\n", argv[3]); return 1; }
    if (strcmp(argv[1], "header") == 0)
        fprintf(out, "#pragma once\n#define GEN_VALUE %d\nint gen_value(void);\n", value);
    else
        fprintf(out, "#include \"gen.h\"\nint gen_value(void) { return GEN_VALUE; }\n");
    fclose(out);
    return 0;
}
C
cat > $R/user.c <<'C'
#include "gen.h"
int user_value(void) { return GEN_VALUE * 1000 + gen_value(); }
C
cat > $R/main.c <<'C'
#include <stdio.h>
int user_value(void);
int main(void) { printf("VALUE=%d\n", user_value()); return 0; }
C

# Builds :main and prints its VALUE (header value * 1000 + source value).
run() { # extra build args...
  rm -f .anubis-out/${MODE}_*/tmp_generated_files_test_main_*/bin/main$EXE
  sleep 0.1  # a file written just before a compile starts counts as changed during it (#191)
  if ! $ANUBIS build --no-tui -m //mode:$MODE -m //mode:$MODE2 -t $T "$@" > $LOG 2>&1; then
    echo "BUILD_FAILED"; tail -5 $LOG >&2; return
  fi
  .anubis-out/${MODE}_*/tmp_generated_files_test_main_*/bin/main$EXE 2>&1 | tr -d '\r' | grep -o 'VALUE=[0-9]*'
}
check() { # name expect build-args...
  local name=$1 want=$2; shift 2
  local r; r=$(run "$@")
  if [ "$r" = "VALUE=$want" ]; then ok "$name: $r"; else bad "$name: $r (want VALUE=$want)"; fi
}

echo 42 > $R/value.txt
check fresh 42042 --no-cache --workers 16
echo 7 > $R/value.txt
check input-changed 7007
rm -rf $R/gen
check fresh-again 7007 --workers 16

# One output directory for the command, whatever the number of modes.
n=$(ls -d .anubis-out/*/tmp_generated_files_test_generate_* 2>/dev/null | wc -l | tr -d ' ')
if [ "$n" = 1 ]; then ok "one-command-job: 1 output directory"; else bad "one-command-job: $n output directories"; fi

rm -rf .anubis-out/${MODE}_*/tmp_generated_files_test_missing_*
if $ANUBIS build --no-tui -m //mode:$MODE -t //$R:missing > $LOG 2>&1; then
  bad "missing-source: build succeeded"
elif grep -q "source files do not exist" $LOG && grep -q "gen/not_generated.c" $LOG; then
  ok "missing-source: fails with a clear message"
else
  bad "missing-source: failed without the message"; tail -5 $LOG
fi

echo "$pass passed, $fail failed"
[ $fail = 0 ]
