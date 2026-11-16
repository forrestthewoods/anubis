#!/usr/bin/env bash
# Package every sample executable this host built, one tarball per target OS,
# for the run-<os> CI jobs (tools/ci_run_samples.sh). Usage, from the repo root
# after the builds:
#
#   tools/ci_collect.sh <host>     # host: linux, windows or macos
#
# Writes dist/exes-<target>-from-<host>.tar for target linux, windows, macos.
# Each holds <target>/<mode>-<sample>/ (a whole staged bin/ dir: the
# executable plus its .so/.dll/.dylib and .dSYM) and, for macOS,
# <mode>-ObjcSample.app. A tar keeps the executable bit and symlinks, which
# upload-artifact drops.
set -euo pipefail

host=${1:?usage: tools/ci_collect.sh <host>}
rm -rf dist
mkdir -p dist

for os in linux windows macos; do
    case $os in
        linux)   prefix=linux_ exe=main ;;
        windows) prefix=win_   exe=main.exe ;;
        macos)   prefix=macos_ exe=main ;;
    esac
    stage=dist/stage/$os
    mkdir -p "$stage"
    n=0
    # .anubis-out/<mode>_<hash>/samples/<group>/<sample>/main/bin/<exe>.
    # Only basic/ and macos/: tools/cache_shadow_test.sh leaves its scratch
    # samples' outputs behind.
    for f in .anubis-out/${prefix}*/samples/{basic,macos}/*/main/bin/$exe; do
        [ -e "$f" ] || continue
        mode=$(echo "$f" | cut -d/ -f2 | sed 's/_[0-9a-f]*$//')
        sample=$(echo "$f" | cut -d/ -f5)
        cp -R "$(dirname "$f")" "$stage/$mode-$sample"
        n=$((n + 1))
    done
    if [ "$os" = macos ]; then
        for app in .anubis-out/macos_*/samples/macos/objc/ObjcSample/app/ObjcSample.app; do
            [ -e "$app" ] || continue
            mode=$(echo "$app" | cut -d/ -f2 | sed 's/_[0-9a-f]*$//')
            cp -R "$app" "$stage/$mode-ObjcSample.app"
            n=$((n + 1))
        done
    fi
    if [ "$n" -eq 0 ]; then
        echo "error: no $os executables built on $host" >&2
        exit 1
    fi
    tar -cf "dist/exes-$os-from-$host.tar" -C dist/stage "$os"
    echo "dist/exes-$os-from-$host.tar: $n"
    ls "$stage"
done
rm -rf dist/stage
