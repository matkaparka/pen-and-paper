#!/usr/bin/env bash
# 对一个已经摆好状态的绘画程序跑完整的合成笔画测试集。
#
#   tools/run-suite.sh <exe 名> <X,Y,W,H> <输出目录> [撤销按键，默认 ctrl+z]
#
# 前提见 tools/README.md：区域整个落在空白浅色画布内，缩放 100%，深色硬边笔刷约 10 像素。
# 每条笔画测完后发一次撤销把画布清回空白；下一条开始前工具会检查路径上是否确实是空白。
# 运行期间会接管笔指针和前台窗口。按住 Esc 中止。
set -u
EXE="$1"; RECT="$2"; OUT="$3"; UNDO="${4:-ctrl+z}"
SB="$(dirname "$0")/target/release/strokebench.exe"
mkdir -p "$OUT"
LOG="$OUT/suite.log"
: > "$LOG"

"$SB" front --expect-exe "$EXE" >> "$LOG" 2>&1 || { echo "无法把 $EXE 切到前台，见 $LOG"; exit 2; }

# one <目录名> <撤销次数> <strokebench run 的其余参数...>
one() {
    local name="$1" undos="$2"; shift 2
    echo "== $name" >> "$LOG"
    "$SB" run --rect "$RECT" --expect-exe "$EXE" --out "$OUT/$name" "$@" >> "$LOG" 2>&1
    local rc=$?
    if [ $rc -ne 0 ]; then
        echo "FAIL $name (exit $rc): $(tail -n 1 "$LOG")"
        [ $rc -eq 2 ] && grep -q "Esc" "$LOG" && exit 2
    else
        echo "ok   $name"
    fi
    "$SB" key --keys "$UNDO" --expect-exe "$EXE" --repeat "$undos" >> "$LOG" 2>&1
    sleep 0.3
}

# 1. 延迟分布：同一条恒压直线重复 10 次
for i in 01 02 03 04 05 06 07 08 09 10; do
    one "line-1000-$i" 1 --case line --pressure 1.0 --speed 1000
done

# 2. 不同笔速
for v in 200 3000; do
    for i in 1 2 3; do
        one "line-$v-$i" 1 --case line --pressure 1.0 --speed "$v"
    done
done

# 3. 压感与起收笔
for c in ramp ramp-up ramp-down flick; do
    one "$c" 1 --case "$c" --pressure 1.0
done

# 4. 横向抖动：三种幅度（10 Hz），外加两种频率（幅度 3 像素）
for a in 1.5 3 6; do
    one "jitter-a$a-f10" 1 --case jitter --pressure 1.0 --amp "$a" --freq 10
done
for f in 4 20; do
    one "jitter-a3-f$f" 1 --case jitter --pressure 1.0 --amp 3 --freq "$f"
done

# 5. 形状
for c in zigzag circle spiral hold-move; do
    one "$c" 1 --case "$c" --pressure 1.0
done
one "dots" 6 --case dots

echo "done: $OUT"
