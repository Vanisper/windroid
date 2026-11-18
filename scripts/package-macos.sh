#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "Usage: $0 [OUTPUT_DIR [RUST_BINARY]]"
    echo 'OUTPUT_DIR defaults to the repository dist directory.'
    echo 'Build a release binary (or use the supplied executable) and the current nsh UI.'
    echo 'Sign and verify OUTPUT_DIR/Windroid.app before publishing it.'
    echo 'Uses the local nsh CLI, Python 3, macOS developer tools, and Cargo for a Rust build.'
}
die() { echo "package-macos: $*" >&2; exit 1; }

if [[ ${1:-} == --help || ${1:-} == -h ]]; then usage; exit 0; fi
root=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
if [[ $# == 0 ]]; then set -- "$root/dist"; fi
[[ ( $# == 1 || $# == 2 ) && -n $1 ]] || { usage >&2; exit 2; }
[[ $(uname -s) == Darwin ]] || die 'requires macOS'
mkdir -p -- "$1"
out=$(cd -- "$1" && pwd -P)
work=$(mktemp -d "$out/.windroid-package.XXXXXXXX")
trap 'rm -rf -- "${work:?}"' EXIT
bundle="$work/Windroid.app"
contents="$bundle/Contents"
ui="$contents/Resources/ui"
mkdir -p "$contents/MacOS" "$ui" "$work/app"

if [[ $# == 2 ]]; then
    binary=$2
else
    # Cargo reports the actual executable even with a configured target triple/target dir.
    (cd "$root" && cargo build --locked --release -p windroid --bin windroid \
        --message-format=json-render-diagnostics) > "$work/artifacts.jsonl"
    binary=$(python3 -c '
import json, sys
for artifact in map(json.loads, sys.stdin):
    if artifact.get("executable") and artifact["target"]["name"] == "windroid":
        print(artifact["executable"])
        break
else:
    sys.exit("Cargo did not report a windroid executable")' < "$work/artifacts.jsonl")
fi
[[ -f $binary && -x $binary ]] || die "missing executable: $binary"
[[ $(/usr/bin/file -Lb -- "$binary") == *Mach-O*executable* ]] || die "expected a Mach-O executable: $binary"
/usr/bin/install -m 0755 "$binary" "$contents/MacOS/windroid"

# nsh writes declarations next to its source even with -o. Build a clean snapshot so
# no stale output is shipped and a packaging run does not change the working tree.
/usr/bin/rsync -a --exclude='/.nsh/' --exclude='.windroid-package.*/' --exclude='Windroid.app/' \
    "$root/app/" "$work/app/"
nsh build --release "$work/app" -o "$ui"
for required in bundle.js capabilities.json; do
    [[ -s $ui/$required ]] || die "nsh did not produce $required"
done

config="$work/app/nsh.json"
info="$contents/Info.plist"
plist_name=$(plutil -extract name raw "$config")
identifier=$(plutil -extract id raw "$config")
version=$(plutil -extract version raw "$config")
build=$(plutil -extract build raw "$config")
plutil -create xml1 "$info"
plutil -insert CFBundleInfoDictionaryVersion -string '6.0' "$info"
plutil -insert CFBundlePackageType -string APPL "$info"
plutil -insert CFBundleExecutable -string windroid "$info"
plutil -insert CFBundleIdentifier -string "$identifier" "$info"
plutil -insert CFBundleName -string "$plist_name" "$info"
plutil -insert CFBundleShortVersionString -string "$version" "$info"
plutil -insert CFBundleVersion -string "$build" "$info"
plutil -insert NSHighResolutionCapable -bool true "$info"
plutil -lint "$info" >&2
codesign --force --sign - --timestamp=none "$bundle"
codesign --verify --deep --strict "$bundle"

# Publish only the completed, signed bundle. On APFS a swap also preserves the last
# working build if replacement fails and leaves no interval with a partially copied app.
destination="$out/Windroid.app"
python3 - "$bundle" "$destination" "$identifier" <<'PY'
import ctypes
import errno
import os
import plistlib
import sys
from pathlib import Path

source, destination = map(Path, sys.argv[1:3])
try:
    os.rename(source, destination)
except OSError as exc:
    if exc.errno not in (errno.ENOTEMPTY, errno.EEXIST):
        raise
    with (destination / 'Contents/Info.plist').open('rb') as handle:
        existing_id = plistlib.load(handle).get('CFBundleIdentifier')
    if existing_id != sys.argv[3]:
        sys.exit(f'Different bundle already at {destination}')
    # Darwin <sys/stdio.h>: renamex_np(..., RENAME_SWAP).
    if ctypes.CDLL(None, use_errno=True).renamex_np(os.fsencode(source), os.fsencode(destination), 2):
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(destination))
PY
printf '%s\n' "$destination"
