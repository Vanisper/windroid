#!/bin/sh
# 仅在夹具的临时目录记下调用，实际协议由测试的 loopback server 提供。
set -eu
dir=$(dirname "$0")
[ "$1" = '-s' ] && [ "$2" = 'test-serial' ] || exit 17
shift 2
printf '%s\n' "$*" >> "$dir/calls"
case "$1 $2" in
    'shell mkdir' | 'push '* )
        if [ -f "$dir/fail-push" ] && [ "$1" = 'push' ]; then echo 'push refused' >&2; exit 1; fi
        ;;
    'forward --no-rebind')
        [ "$3" = tcp:0 ] || exit 18
        read -r port < "$dir/next-port"
        printf 'test-serial tcp:%s %s\n' "$port" "$4" > "$dir/map-$port"
        echo "$port"
        ;;
    'forward --list')
        for map in "$dir"/map-*; do [ ! -f "$map" ] || cat "$map"; done
        ;;
    'forward --remove')
        port=$(printf '%s' "$3" | cut -d: -f2)
        rm "$dir/map-$port"
        ;;
    'shell -T')
        if [ -f "$dir/exit-server" ]; then echo '[server] ERROR: codec unavailable' >&2; exit 13; fi
        echo '[server] INFO: New display: 32x32/240 (id=57)'
        exec /bin/sleep 30
        ;;
    'shell p='*) ;;
    *) exit 19 ;;
esac
