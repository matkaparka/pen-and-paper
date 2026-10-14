"""把 run-suite.sh 的输出目录汇总成一张表。只用标准库。

用法: python tools/summarize.py <套件输出目录> [更多目录...]
"""
import json
import statistics
import sys
from pathlib import Path


def load(d: Path):
    runs = {}
    for p in sorted(d.glob("*/summary.json")):
        try:
            runs[p.parent.name] = json.loads(p.read_text(encoding="utf-8"))
        except (OSError, ValueError) as e:
            print(f"  跳过 {p}: {e}")
    return runs


def med(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def rng(values):
    values = [v for v in values if v is not None]
    return (min(values), max(values)) if values else (None, None)


def fmt(v, nd=1):
    return "-" if v is None else f"{v:.{nd}f}"


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    for arg in sys.argv[1:]:
        d = Path(arg)
        runs = load(d)
        if not runs:
            print(f"{d}: 没有找到 summary.json")
            continue
        any_run = next(iter(runs.values()))
        print(f"\n# {d}  target={any_run.get('target_exe')}  runs={len(runs)}")

        print("\n## 延迟（恒压直线，按笔速分组；单位 ms，括号内为各次运行的最小–最大）")
        print("speed  n  first_ink        area_lag_median   area_lag_p90      point_lag_median  update_hz  frame_gap_max")
        for speed in ("200", "1000", "3000"):
            g = [r for n, r in runs.items() if n.startswith(f"line-{speed}-")]
            if not g:
                continue
            fi = [r.get("first_ink_ms") for r in g]
            tl = [r.get("area_lag_ms_median") for r in g]
            tp = [r.get("area_lag_ms_p90") for r in g]
            pl = [r.get("track_lag_ms_median") for r in g]
            print(
                f"{speed:>5} {len(g):>2}  "
                f"{fmt(med(fi))} ({fmt(rng(fi)[0])}–{fmt(rng(fi)[1])})   "
                f"{fmt(med(tl))} ({fmt(rng(tl)[0])}–{fmt(rng(tl)[1])})   "
                f"{fmt(med(tp))} ({fmt(rng(tp)[0])}–{fmt(rng(tp)[1])})   "
                f"{fmt(med(pl))}   "
                f"{fmt(med([r.get('update_rate_hz') for r in g]))}   "
                f"{fmt(max(v for v in (r.get('frame_interval_ms_max') for r in g) if v is not None))}"
            )

        print("\n## 收笔（恒压直线 1000 px/s）")
        g = [r for n, r in runs.items() if n.startswith("line-1000-")]
        if g:
            print(f"tail_missing_px 中位 {fmt(med([r.get('tail_missing_px') for r in g]))}，"
                  f"抬笔后墨迹画完用时 中位 {fmt(med([r.get('ink_complete_after_up_ms') for r in g]))} ms，"
                  f"线宽 中位 {fmt(med([r.get('ink_width_px_mid_mean') for r in g]), 2)} px")

        print("\n## 横向抖动的保留比例（成品重心 RMS / 输入 RMS）")
        for n, r in runs.items():
            if n.startswith("jitter"):
                i, o = r.get("in_y_rms_px"), r.get("ink_centroid_y_rms_px")
                ratio = o / i if i and o is not None else None
                print(f"{n:<16} in={fmt(i, 3)} out={fmt(o, 3)} ratio={fmt(ratio, 3)}")

        print("\n## 其余用例")
        for n, r in runs.items():
            if n.startswith(("line-", "jitter")):
                continue
            print(
                f"{n:<10} first_ink={fmt(r.get('first_ink_ms'))} width_max={fmt(r.get('ink_width_px_max'), 2)} "
                f"ink_x={r.get('ink_x_start', '-')}..{r.get('ink_x_end', '-')} "
                f"(in {r.get('in_x_start', '-')}..{r.get('in_x_end', '-')}) dark_px={r.get('final_dark_px')}"
            )


if __name__ == "__main__":
    main()
