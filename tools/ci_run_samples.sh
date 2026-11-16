#!/usr/bin/env bash
# Run every sample executable built for this OS, from every host, and print a
# pass/fail table. Usage, on a runner of the target OS:
#
#   tools/ci_run_samples.sh <os> <dir>    # os: linux, windows or macos
#
# <dir> holds the exes-<os>-from-<host>.tar files tools/ci_collect.sh wrote.
# Runs everything before failing, so one bad host/target pair doesn't hide
# the others. simple_cpp is skipped: it's a memory benchmark (minutes of
# subprocesses), built but never run.
set -uo pipefail

os=${1:?usage: tools/ci_run_samples.sh <os> <dir>}
dir=${2:?usage: tools/ci_run_samples.sh <os> <dir>}

# A hung exe (a CRT assertion dialog on Windows) must not eat the whole job.
# macOS has no timeout(1); the step's timeout-minutes covers it there.
# (Empty arrays are expanded with ${a[@]+...}: macOS's bash 3.2 calls them
# unbound under set -u.)
to=()
if timeout --version >/dev/null 2>&1; then to=(timeout 120); fi

results=()
failed=0

# record <PASS|FAIL> <host> <name> [detail]
record() {
    results+=("$1  $os from $2  $3${4:+  ($4)}")
    if [ "$1" = FAIL ]; then failed=1; fi
}

shopt -s nullglob
tars=("$dir"/exes-"$os"-from-*.tar)
if [ ${#tars[@]} -eq 0 ]; then
    echo "error: no exes-$os-from-*.tar in $dir" >&2
    exit 1
fi

for tarf in "${tars[@]}"; do
    host=$(basename "$tarf" .tar)
    host=${host##*-from-}
    root=run/$host
    rm -rf "$root"
    mkdir -p "$root"
    tar -xf "$tarf" -C "$root"
    for entry in "$root/$os"/*; do
        name=$(basename "$entry")
        case $name in *simple_cpp*) continue ;; esac
        echo
        echo "=== $os from $host: $name"
        case $name in
            *.app) exe=$entry/Contents/MacOS/main ;;
            *) if [ "$os" = windows ]; then exe=$entry/main.exe; else exe=$entry/main; fi ;;
        esac
        if [ ! -f "$exe" ]; then
            record FAIL "$host" "$name" "no executable"
            continue
        fi
        # Windows ignores the bit; tars written on Windows may not carry it.
        [ "$os" = windows ] || chmod +x "$exe"
        case $os in
            linux) ldd "$exe" || true ;;
            macos) otool -L "$exe" || true ;;
        esac
        log=$root/$name.log
        ${to[@]+"${to[@]}"} "$exe" > "$log" 2>&1
        status=$?
        cat "$log"
        if [ $status -ne 0 ]; then
            record FAIL "$host" "$name" "exit $status"
            continue
        fi
        if [[ $name == *.app ]] && ! grep -q 'Bundle dev.anubis.samples.objc resource: Hello' "$log"; then
            record FAIL "$host" "$name" "bundle resource not found"
            continue
        fi
        if [ "$os" = macos ] && [[ $name != *.app ]]; then
            # The dSYM must match the binary (dwarfdump --uuid prints both)...
            a=$(dwarfdump --uuid "$exe" | awk '{print $2}')
            b=$(dwarfdump --uuid "$exe.dSYM" 2>/dev/null | awk '{print $2}')
            if [ -z "$a" ] || [ "$a" != "$b" ]; then
                record FAIL "$host" "$name" "dSYM mismatch: '$a' vs '$b'"
                continue
            fi
            # ...and hold real debug info: a debug build's dSYM names main().
            if [[ $name == macos_dev-trivial_c ]] &&
                ! dwarfdump --name main "$exe.dSYM" | grep -q DW_TAG_subprogram; then
                record FAIL "$host" "$name" "dSYM has no main()"
                continue
            fi
        fi
        record PASS "$host" "$name"
    done
done

echo
echo "=== Results ($os)"
printf '%s\n' ${results[@]+"${results[@]}"} | sort
exit $failed
