# 测量工具

这里是我们自己写的测量工具，用来把"手感"变成可重复的数字。它们只做外部观察：
发送标准的系统输入、抓屏幕画面、读进程的公开状态，不注入代码，不读目标程序的内存或文件。

构建：`cargo build --release`（在本目录下），产物在 `target\release\`。

## pentap：笔输入记录器

开一个白底窗口，把系统送来的每个笔输入样本原样写进 CSV（时间戳、像素与 himetric 坐标、压力、倾斜、标志位），同时立即画线。

```
pentap [--api pointer|wintab] --out FILE.csv [--pos X,Y] [--size W,H] [--max-width PX] [--topmost] [--duration SEC]
pentap --wintab-info
```

用途：
- 记录真实数位板的原始特性：报告率、压力分级、静止时的坐标抖动。
- 作为 strokebench 的标定靶子：它的线宽是已知函数 `round(1 + max_width × 压力)`。
- 作为延迟下限的参照：收到事件立刻用 GDI 画，没有任何缓冲或平滑。

两种输入接口：

- `--api pointer`（默认）：Windows 指针输入（WM_POINTER），含系统合并掉的历史样本。已用合成输入验证。
- `--api wintab`：Wintab 数据包，坐标是数位板原生分辨率。`--wintab-info` 只打印驱动和设备信息（设备名、报告率、坐标与压力范围）。探测路径已验证；数据包记录路径还没有用真实的笔验证过。

## strokebench：合成笔画注入 + 带时间戳抓屏

```
strokebench list
strokebench probe
strokebench shot --out F.png [--rect X,Y,W,H] [--div N]
strokebench run --case NAME --rect X,Y,W,H --expect-exe NAME.exe --out DIR
                [--rate 200] [--speed 1000] [--pressure 0.6] [--amp 3] [--freq 10] [--settle-ms 1000]
```

`run` 向屏幕上的一块矩形区域注入一条确定性的笔画（固定轨迹、固定压力序列、固定采样率），同时用 Desktop Duplication 抓这块区域的每一帧，记录系统给出的呈现时间戳。输出目录里有：

| 文件 | 内容 |
|------|------|
| `samples.csv` | 每个注入样本的计划时刻、实际时刻、坐标、压力 |
| `frames.csv` | 每一帧的呈现时刻、变化像素数、有墨像素数、墨迹前沿到了第几个样本 |
| `before.png` / `after.png` | 落笔前和笔画结束后的区域画面 |
| `profile.csv` | 仅水平笔画：逐列的等效粗细、最大浓度、墨迹重心，以及对应的输入压力 |
| `summary.json` | 汇总指标 |

主要指标：

- `first_ink_ms`：落笔事件注入完成 → 第一帧出现墨迹。落笔前会先悬停 150 ms，让目标程序自绘的笔刷光标先出现并计入底噪，否则光标会被误当成墨迹。
- `area_lag_ms_*`：**跟随延迟的首选指标**，只对 `line` 用例输出。按"还没画出来的长度 = 还差的墨迹面积 ÷ 线宽"算出墨迹落后笔多少，包含程序自身的平滑/稳定器造成的滞后。在无平滑的参照程序上，它的读数与笔速无关（3.1–3.8 ms）。
- `track_lag_ms_*`：按"轨迹点周围是否有墨"逐点判定的跟随延迟。笔刷有半径，墨迹前缘比最新笔触的中心靠前，所以它在慢速下偏小，仅作对照。
- `update_rate_hz`、`frame_interval_ms_*`：笔画进行中目标程序实际更新画面的频率和间隔。
- `tail_missing_px`、`ink_complete_after_up_ms`：收笔处墨迹有没有画到头，抬笔后多久才画完。
- `ink_width_px_*`、`ink_centroid_y_rms_px` 与 `in_y_rms_px`：粗细，以及输入的横向抖动在成品里剩下多少。

使用前提（不满足时结果无意义）：

- `--rect` 整个落在目标程序的空白浅色画布内；画布缩放 100%、不旋转；墨色为深色。
- 跟随延迟和收笔指标要求笔迹至少 5 像素粗、浓度足够（判定条件是轨迹点周围 5×5 像素里至少 12 个明显变暗）。细笔画或低压力段会被判成"没出墨"。
- 运行期间目标窗口必须在前台。工具在每条笔画落笔前都会确认区域的四角和中心属于 `--expect-exe` 指定的程序，否则中止。按住 Esc 可随时中止。

方法的固有边界：

1. 合成输入走 Windows 指针输入栈，不经过数位板硬件和厂商的 Wintab 驱动。目标程序若只认 Wintab，就拿不到这里注入的压力。
2. 注入坐标是整数像素；真实数位板有亚像素精度。
3. 时间戳是合成器提交帧的时刻，不含显示器扫描和面板响应。测出的是软件侧延迟，比眼睛看到的端到端延迟短一个大致恒定的量；同一台机器上不同软件之间可以直接比。
4. 时间分辨率是一个刷新周期（240 Hz 下约 4.2 ms）。

自检与标定结果记录在 `analysis/stage1/tool-selftest.md`（本地，不入库）。

摆测试状态用的辅助命令（都带 `--expect-exe` 检查，只对指定程序的窗口生效）：

```
strokebench front --expect-exe NAME.exe                     把该程序的窗口切到前台
strokebench tap   --at X,Y [--to X,Y] [--with-keys space] --expect-exe NAME.exe   用合成笔点一下或拖一段，可同时按住修饰键
strokebench click --at X,Y [--right] [--double] --expect-exe NAME.exe   用合成鼠标点击（有些控件不响应合成笔）
strokebench scroll --at X,Y --delta 120 --expect-exe NAME.exe          滚轮，正数向上
strokebench key   --keys ctrl+z --expect-exe NAME.exe [--repeat N]
```

### bg：不抢前台的后台驱动

```
strokebench bg --exe NAME.exe [--title 标题里的一段] --do "步骤;步骤;…"
```

直接给目标程序的窗口发鼠标、键盘和菜单消息，再用 PrintWindow 取它自己画出来的画面。窗口不用在前台，系统指针不动，用户正在用电脑时也能跑。坐标都是窗口客户区内的物理像素。

| 步骤 | 作用 |
|------|------|
| `size` | 打印客户区大小 |
| `grab:FILE.png` 或 `grab:FILE.png@2` | 存一张客户区画面，`@N` 为缩小倍数 |
| `click:X,Y`、`dblclick:X,Y`、`hover:X,Y` | 鼠标左键点一下、双击、只移过去 |
| `drag:X1,Y1>X2,Y2*步数` | 按下、分若干步移动、松开 |
| `timedrag:X1,Y1>X2,Y2*步数` | 同上，但每一步之后让窗口立刻重画，并报告每一步耗时的均值、中位数、95 分位和最大值 |
| `down:X,Y`、`move:X,Y`、`up:X,Y` | 分开发，用来拖折线 |
| `key:NAME`、`keydown:NAME`、`keyup:NAME` | 按键（字母、数字、enter、esc、tab、delete、home、f1–f12 等） |
| `cmd:ID`、`postcmd:ID` | 菜单命令；会弹对话框的用 `postcmd`，否则这边会等到对话框关掉 |
| `postkey:NAME` | 投递（而不是发送）一次按键：对话框在自己的消息循环里截按键时要用它；配合 `--title` 指定对话框窗口 |
| `close`、`title` | 请窗口关闭（等同点关闭按钮）；打印窗口标题 |
| `wait:毫秒` | 停一会儿 |

边界：这不是真实的输入通路。没有压感（走的是鼠标路径，压力恒为满）；修饰键按不下去，因为程序查询的是真实键盘状态，所以带 Ctrl、Shift、Alt 的操作要用菜单命令代替；量不了跟随延迟。只用它检查我们自己的程序"点了这里、拖了这一段之后画面对不对"，以及重画一帧要多久。

什么时候用它、什么时候用 `run`：先看用户是不是正在用电脑。

```
powershell -ExecutionPolicy Bypass -File idle.ps1 -Seconds 90 -Timeout 600
```

`idle.ps1` 等到键盘鼠标连续空闲这么多秒才返回 0，超时返回 1，并打印当时的前台窗口。空闲了才跑会接管指针的 `run`、`tap`、`click`；不空闲就用 `bg`。启动测试用的程序实例时，把 `APPDATA` 指到一个临时目录，免得改掉用户自己的设置；不要带着会在启动时弹窗的参数启动（例如直接打开一个会给出导入说明的 PSD），那样窗口会抢到前台。

`pngpx.py` 是配套的小模块：`from pngpx import load` 读出截图的像素，验收脚本用它数像素、量宽度。

`area_lag_ms` 的已知偏差：

- 目标程序如果在画面里自绘笔刷光标，光标压在墨迹上的那部分会让面积法少算一小段墨迹。少算的长度是固定的几个像素，所以笔速越慢折合的时间越长：200 像素/秒下可达十几毫秒，1000 像素/秒下不超过约 3 毫秒。比较不同程序时用 1000 像素/秒及以上的数值。
- 面积换算成长度用的是"线的中段各列墨量的平均值"（`line_dark_per_px_mean`）。用过另外两种算法，各有偏差：中位数在旋转视图下把延迟高估十几毫秒；"总面积 ÷ 总长度"在起笔渐细较长时（稳定器高档位）高估约 10 毫秒。水平、不旋转、不渐细的线三种算法结果相同。

已知的坑：

- `shot` 为了逼合成器出帧会短暂显示一个 2×2 的小窗口。GTK 程序（GIMP）的弹出菜单会因此关闭，所以菜单打开后不要在点菜单项之前截图。
- `click` 和 `tap` 会先把目标点所在的顶层窗口切到前台。GIMP 的弹出菜单本身是一个顶层窗口，这样做会让菜单关闭，后续点击落到画布上。在 GIMP 里选菜单项请用 `key`（Alt+助记键，再用方向键和回车）。
- 参照软件的菜单栏按一次 Esc 只关下拉菜单，要按两次才完全退出菜单栏。

批量脚本：

- `run-suite.sh <exe> <X,Y,W,H> <输出目录>`：完整测试集，30 条笔画，约 4 分钟。
- `run-mini.sh <exe> <X,Y,W,H> <输出目录>`：精简测试集（延迟和平滑），9 条笔画，用来在同一程序里扫某个参数。
- `python summarize.py <输出目录>...`：把一个或多个输出目录汇总成表。

## filedlg.ps1：填系统的文件对话框（配合 bg）

```
powershell -ExecutionPolicy Bypass -File filedlg.ps1 [-ProcessName zhibi-test] (-Path 完整路径 [-Replace] | -Cancel) [-Kind N]
```

`bg` 用 `postcmd:` 打开"打开 / 另存为 / 导出 / 导入"之后，用它等对话框出来，打印标题、默认的文件名和选着的文件类型，再把文件名填进去按"确定"（`-Cancel` 是取消）。`-Replace` 在系统问"是否替换"时答"是"，`-Kind N` 先选第 N 种文件类型。全靠窗口消息（`WM_SETTEXT`、`WM_COMMAND`、`TDM_CLICK_BUTTON`），不动指针、不按键盘。

为什么不用 UI 自动化：这种对话框下半截的文件名框、类型框、按钮在 UI 自动化里只是没有模式的"窗格"，设不了值；它们是真正的子窗口，发消息可以。另存为对话框的文件名框埋得深（`Edit` 1001 在一个没有编号的 `ComboBox` 里），类型框没有编号，脚本按"不是装着文件名的那个 `ComboBox`"去找。对话框开着的时候，`bg` 找到的"程序的窗口"是对话框，这时发给主窗口的 `postcmd` 会落空。

## appstat.ps1：启动耗时与空闲内存

```
powershell -File appstat.ps1 -Exe PATH [-ProcessName NAME] [-TitleRegex RE] [-Runs 5] [-SettleSeconds 5] [-Out FILE.csv]
```

每轮记录：启动到主窗口出现的时间、到消息循环空闲的时间、静置后的工作集与私有内存、线程数、句柄数。
这个脚本自身有开销：用最简单的窗口程序实测，"主窗口出现"读数在 180–230 ms。它适合比较秒级的启动差异，不适合分辨几十毫秒的差别。
