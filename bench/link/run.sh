#!/usr/bin/env bash
# Link benchmark: lld-link vs RadLinker on one big Windows link (#28).
# Builds bench/link/gen (see gen.py) for //mode:win_dev, then re-runs its
# exact link command with each linker. Needs Windows and installed toolchains.
#   bench/link/run.sh [num_tus]     RUNS=5 by default
set -euo pipefail
cd "$(dirname "$0")/../.."
export MSYS_NO_PATHCONV=1

N=${1:-1000}
RUNS=${RUNS:-5}
RADLINK_URL=https://github.com/EpicGamesExt/raddebugger/releases/download/v0.9.29-alpha/radlink.exe
RADLINK_SHA256=ce5b5106392a49548f727ac37f86d38c4f5a15ae40e1f49a5a3cfd6ea632401a

work=.anubis-temp/link-bench
rm -rf "$work" && mkdir -p "$work"

curl -sSfL -o "$work/radlink.exe" "$RADLINK_URL"
echo "$RADLINK_SHA256  $work/radlink.exe" | sha256sum -c -

python bench/link/gen.py "$N"
echo "=== build (compiles + one lld-link link)"
start=$(date +%s)
./target/release/anubis.exe -l trace build --no-tui --no-cache -m //mode:win_dev -t //bench/link/gen:main \
    > "$work/build.log" 2>&1 || { tail -40 "$work/build.log"; exit 1; }
echo "built in $(( $(date +%s) - start ))s"

# The link Anubis ran (lld-link @response-file), from its trace log.
line=$(sed 's/\x1b\[[0-9;]*m//g' "$work/build.log" | grep -a 'Executing command: .*lld-link' | tail -1 | tr -d '\r')
cmd=${line#*Executing command: }
lld=${cmd%% *}
rsp=${cmd#* @}
grep -v '^/OUT:' "$rsp" > "$work/args.rsp"
echo "link inputs: $(grep -c '\.obj$' "$work/args.rsp") objects, args: $(wc -l < "$work/args.rsp")"

time_linker() {  # name exe
    local name=$1 exe=$2 out="$work/$1.exe" times=()
    for _ in $(seq "$RUNS"); do
        rm -f "$out" "${out%.exe}.pdb"
        local s e
        s=$(date +%s%N)
        "$exe" @"$work/args.rsp" "/OUT:$out" > "$work/$name.log" 2>&1 || { echo "$name failed:"; cat "$work/$name.log"; return 1; }
        e=$(date +%s%N)
        times+=($(( (e - s) / 1000000 )))
    done
    local sorted
    sorted=$(printf '%s\n' "${times[@]}" | sort -n | tr '\n' ' ')
    printf '%-9s runs(ms): %s | exe %s KB, pdb %s KB | %s\n' "$name" "$sorted" \
        "$(( $(stat -c %s "$out") / 1024 ))" "$(( $(stat -c %s "${out%.exe}.pdb" 2>/dev/null || echo 0) / 1024 ))" \
        "$("$out")"
}

echo "=== $(nproc) logical CPUs, $RUNS runs each, sorted"
time_linker lld-link "$lld"
time_linker radlink "$work/radlink.exe"
