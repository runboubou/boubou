#!/bin/sh
set -eu

case "${1-}" in
    '') run_now=false ;;
    --run) run_now=true ;;
    *) printf '%s\n' 'Usage: sh install.sh [--run]' >&2; exit 2 ;;
esac
[ "$#" -le 1 ] || { printf '%s\n' 'Unexpected installer arguments.' >&2; exit 2; }
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || {
    printf '%s\n' 'This development package requires an Apple Silicon Mac.' >&2
    exit 1
}
package_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
install_dir=${BOUBOU_INSTALL_DIR:-"$HOME/.local/bin"}
case "$install_dir" in
    /*) ;;
    *) printf '%s\n' 'BOUBOU_INSTALL_DIR must be an absolute directory.' >&2; exit 2 ;;
esac
if [ -L "$install_dir/boubou" ] || { [ -e "$install_dir/boubou" ] && [ ! -f "$install_dir/boubou" ]; }; then
    printf '%s\n' 'Refusing to replace a symlink or non-file at the Boubou installation path.' >&2
    exit 1
fi
(cd "$package_dir" && /usr/bin/shasum -a 256 -c SHA256SUMS > /dev/null)
mkdir -p "$install_dir"
temporary=$(/usr/bin/mktemp "$install_dir/.boubou-install.XXXXXX")
trap 'rm -f "$temporary"' 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
/bin/cp "$package_dir/boubou" "$temporary"
/bin/chmod 755 "$temporary"
/bin/mv -f "$temporary" "$install_dir/boubou"
printf 'Installed: %s\n' "$install_dir/boubou"
printf 'Run: "%s/boubou" run liquid\n' "$install_dir"
printf '%s\n' 'First use prepares the pinned model locally. This is a development candidate.'
if [ "$run_now" = true ]; then
    exec "$install_dir/boubou" run liquid < /dev/tty
fi
