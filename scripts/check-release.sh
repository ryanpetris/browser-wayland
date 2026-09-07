#!/bin/sh
# Check the installed release and render a frame without GPU or audio services.
set -eu

expected="elsewhere ${1:?expected release version}"
actual=$(elsewhere --version)
if [ "$actual" != "$expected" ]; then
    printf 'Expected %s, got %s\n' "$expected" "$actual" >&2
    exit 1
fi

work=$(mktemp -d)
pid=
cleanup() {
    if [ -n "$pid" ]; then
        kill -KILL "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap 'cleanup' EXIT
trap 'exit 1' HUP INT TERM
export XDG_CONFIG_HOME="$work/config" XDG_RUNTIME_DIR="$work/runtime"
mkdir -m 700 "$XDG_RUNTIME_DIR"
elsewhere --render-node none --software-encoding --codec vp8 --no-audio --no-rtc \
    --no-tls --listen 127.0.0.1:18443 --screen-size 320x240 >"$work/server.log" 2>&1 &
pid=$!
for _ in $(seq 1 30); do
    if ! kill -0 "$pid" 2>/dev/null; then break; fi
    if [ -s "$XDG_CONFIG_HOME/elsewhere/token" ] &&
        curl --fail --silent --max-time 2 \
            -H "Authorization: Bearer $(cat "$XDG_CONFIG_HOME/elsewhere/token")" \
            http://127.0.0.1:18443/api/screenshot.png -o "$work/frame.png"; then
        test -s "$work/frame.png"
        printf 'Release version and software-rendered screenshot verified\n'
        exit 0
    fi
    sleep 1
done
cat "$work/server.log" >&2
exit 1
