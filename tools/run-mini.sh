#!/usr/bin/env bash
# 精简测试集：只测延迟和平滑程度，用于在同一个程序里扫某个参数（例如稳定器档位）。
#
#   tools/run-mini.sh <exe 名> <X,Y,W,H> <输出目录> [撤销按键，默认 ctrl+z]
#
# 前提与 run-suite.sh 相同。
set -u
EXE="$1"; RECT="$2"; OUT="$3"; UNDO="${4:-ctrl+z}"
SB="$(dirname "$0")/target/release/strokebench.exe"
mkdir -p "$OUT"
LOG="$OUT/suite.log"
: > "$LOG"

one() {
    local name="$1"; shift
    echo "== $name" >> "$LOG"
    if "$SB" run --rect "$RECT" --expect-exe "$EXE" --out "$OUT/$name" "$@" >> "$LOG" 2>&1; then
        echo "ok   $name"
    else
        echo "FAIL $name: $(tail -n 1 "$LOG")"
    fi
    "$SB" key --keys "$UNDO" --expect-exe "$EXE" >> "$LOG" 2>&1
    sleep 0.3
}

for i in 1 2 3; do
    one "line-1000-$i" --case line --pressure 1.0 --speed 1000 --settle-ms 1500
done
one "line-200-1" --case line --pressure 1.0 --speed 200 --settle-ms 1500
one "line-3000-1" --case line --pressure 1.0 --speed 3000 --settle-ms 1500
one "jitter-a3-f4" --case jitter --pressure 1.0 --amp 3 --freq 4 --settle-ms 1500
one "jitter-a3-f10" --case jitter --pressure 1.0 --amp 3 --freq 10 --settle-ms 1500
one "jitter-a3-f20" --case jitter --pressure 1.0 --amp 3 --freq 20 --settle-ms 1500
one "zigzag" --case zigzag --pressure 1.0 --settle-ms 1500
echo "done: $OUT"
