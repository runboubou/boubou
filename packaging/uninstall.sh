#!/bin/sh
set -eu
[ "$#" -eq 0 ] || { printf '%s\n' 'Usage: sh uninstall.sh' >&2; exit 2; }
install_dir=${BOUBOU_INSTALL_DIR:-"$HOME/.local/bin"}
case "$install_dir" in
    /*) ;;
    *) printf '%s\n' 'BOUBOU_INSTALL_DIR must be an absolute directory.' >&2; exit 2 ;;
esac
if [ -L "$install_dir/boubou" ]; then
    printf '%s\n' 'Refusing to remove a symlink at the Boubou installation path.' >&2
    exit 1
fi
if [ -e "$install_dir/boubou" ]; then
    version=$("$install_dir/boubou" --version)
    case "$version" in
        'boubou '*) ;;
        *) printf '%s\n' 'The installed file does not identify itself as Boubou.' >&2; exit 1 ;;
    esac
    /bin/rm "$install_dir/boubou"
fi
printf '%s\n' 'Boubou removed. Model caches and saved audio were kept.'
