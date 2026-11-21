#!/bin/bash
# End-to-end test of anubis_cmd caching (src/rules/cmd_rules.rs, #174):
# - a rebuild with nothing changed runs no tool, and deleting the outputs
#   restores them from the cache;
# - editing a file in `srcs` or the tool's source reruns the command, and
#   reverting is a cache hit;
# - deploy_into_workspace writes a deployed file only when it changed;
# - an undeclared output, a missing declared output, and an argument naming a
#   file that isn't in `srcs` each fail the build.
# Run from the repo root: tools/cmd_cache_test.sh [anubis binary]
set -u
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) MODE=win_dev; EXE=.exe; ANUBIS=${1:-./target/release/anubis.exe} ;;
  Darwin) MODE=macos_dev; EXE=; ANUBIS=${1:-./target/release/anubis} ;;
  *) MODE=linux_dev; EXE=; ANUBIS=${1:-./target/release/anubis} ;;
esac
export MSYS_NO_PATHCONV=1
D=tmp_cmd_cache_test
R=samples/$D
T=//samples/$D
trap 'rm -rf $R' EXIT
rm -rf $R; mkdir -p $R
SALT=$RANDOM$RANDOM  # fresh inputs, so earlier runs' cache entries don't match
pass=0; fail=0
LOG=$R/last.log
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; }

cp samples/basic/codegen/gen.c samples/basic/codegen/main.c $R/
echo "/* $SALT */" >> $R/gen.c
printf 'one %s\n' $SALT > $R/greeting.txt
cp samples/basic/codegen/ANUBIS $R/ANUBIS
cat >> $R/ANUBIS <<'EOT'

anubis_cmd(
    name = "undeclared",
    tool = Target(":gen"),
    srcs = [ RelPath("greeting.txt") ],
    args = [ [ RelPath("greeting.txt"), "$OUTDIR/a.c", "a" ], [ RelPath("greeting.txt"), "$OUTDIR/b.c", "b" ] ],
    outputs = [ "a.c" ],
)

anubis_cmd(
    name = "missing",
    tool = Target(":gen"),
    srcs = [ RelPath("greeting.txt") ],
    args = [ [ RelPath("greeting.txt"), "$OUTDIR/a.c", "a" ] ],
    outputs = [ "a.c", "never_written.c" ],
)

# Different content per build type, deployed to one directory: building two
# modes at once must fail instead of flipping the file between them.
anubis_cmd(
    name = "per_mode",
    tool = Target(":gen"),
    srcs = [ RelPath("greeting.txt") ],
    args = [ [ RelPath("greeting.txt"), "$OUTDIR/per_mode.c", select((build_type) => { (debug) = "dbg", (release) = "rel" }) ] ],
    outputs = [ "per_mode.c" ],
)

deploy_into_workspace(
    name = "per_mode_deploy",
    dep = Target(":per_mode"),
    dst = RelPath("generated"),
)

anubis_cmd(
    name = "not_in_srcs",
    tool = Target(":gen"),
    srcs = [],
    args = [ [ RelPath("greeting.txt"), "$OUTDIR/a.c", "a" ] ],
    outputs = [ "a.c" ],
)
EOT

# build -> "<misses> <tools run> <program output>"
build() {
  sleep 0.1  # a file written just before an action starts counts as changed during it (#191)
  if ! $ANUBIS build --no-tui -m //mode:$MODE -t $T:main > $LOG 2>&1; then
    echo "BUILD_FAILED"; tail -5 $LOG >&2; return
  fi
  local misses; misses=$(grep -o '[0-9]* misses' $LOG | tail -1 | cut -d' ' -f1)
  local tools; tools=$(grep -o 'ran [0-9]* tool' $LOG | tail -1 | cut -d' ' -f2)
  local out; out=$(.anubis-out/${MODE}_*/${D}_main_*/bin/main$EXE 2>&1 | tr -d '\r')
  echo "$misses $tools $out"
}
# check name result expected-text expect(run|hit)
check() {
  local r=$2 good=1
  [[ "$r" == *"$3"* ]] || good=0
  local misses=${r%% *}; local rest=${r#* }; local tools=${rest%% *}
  case "$misses$tools" in *[!0-9]*|'') good=0 ;; esac
  if [ "$4" = hit ] && [ "$tools" != 0 ]; then good=0; fi
  if [ "$4" = run ] && [ "$tools" = 0 ]; then good=0; fi
  if [ $good = 1 ]; then ok "$1: $r"; else bad "$1: $r (want \"$3\", $4)"; fi
}
mtime() { stat -c %Y.%Z "$1" 2>/dev/null || stat -f %m.%c "$1"; }

check "cold build" "$(build)" "one $SALT" run
sleep 2  # past the stat cache's window for files written just now
check "no-op rebuild" "$(build)" "one $SALT" hit
DEPLOYED=$R/generated/greeting.c
before=$(mtime $DEPLOYED)
check "no-op rebuild again" "$(build)" "one $SALT" hit
if [ "$(mtime $DEPLOYED)" = "$before" ]; then ok "deploy left an unchanged file alone"; else bad "deploy rewrote an unchanged file"; fi

printf 'two %s\n' $SALT > $R/greeting.txt
check "edit a src" "$(build)" "two $SALT" run
printf 'one %s\n' $SALT > $R/greeting.txt
check "revert a src: everything restored from the cache" "$(build)" "one $SALT" hit

rm -rf .anubis-out/${MODE}_*/${D}_generate_*
r=$(build)
if [ "$(grep -c 'Running.*cmd gen' $LOG)" = 0 ]; then ok "deleted outputs restored from cache: $r"; else bad "deleted outputs: command ran: $r"; fi

echo "/* edited */" >> $R/gen.c
r=$(build)
if [ "$(grep -c 'Running.*cmd gen' $LOG)" = 1 ]; then ok "edit the tool: command reran: $r"; else bad "edit the tool: command did not rerun: $r"; fi

check_fails() { # name target expected-text
  if $ANUBIS build --no-tui -m //mode:$MODE -t $T:$2 > $LOG 2>&1; then
    bad "$1: build succeeded"
  elif grep -q "$3" $LOG; then
    ok "$1: fails with \"$3\""
  else
    bad "$1: failed without \"$3\""; tail -5 $LOG
  fi
}
check_fails "undeclared output" undeclared "not in \`outputs\`: b.c"
check_fails "missing output" missing "did not write these declared outputs"
check_fails "argument not in srcs" not_in_srcs "which is not in \`srcs\`"

REL=${MODE/_dev/_release}
if $ANUBIS build --no-tui -m //mode:$MODE -m //mode:$REL -t $T:per_mode_deploy > $LOG 2>&1; then
  bad "two modes deploying different content: build succeeded"
elif grep -q "deploy different content to the same file" $LOG; then
  ok "two modes deploying different content: fails"
else
  bad "two modes deploying different content: failed without the conflict message"; tail -5 $LOG
fi

echo "anubis_cmd cache test: $pass passed, $fail failed"
[ $fail = 0 ]
