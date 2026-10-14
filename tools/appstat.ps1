# appstat：启动耗时与空闲内存占用。纯外部观察，不向目标进程注入任何东西。
#
# 用法：
#   powershell -File tools\appstat.ps1 -Exe "D:\apps\GIMP\GIMP 3\bin\gimp-3.2.exe" -TitleRegex 'GIMP' -Runs 5 -Out analysis\stage1\startup\gimp.csv
#   powershell -File tools\appstat.ps1 -Exe mspaint.exe -ProcessName mspaint -Runs 5
#
# 每一轮：启动 → 等主窗口出现 → 等消息循环空闲 → 静置 → 读内存 → 请求关闭主窗口。
# 第 1 轮通常是冷启动（磁盘缓存未命中），和后面几轮分开看。
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string[]]$ExeArgs = @(),
    # 启动器和真正的进程不是同一个时（例如商店应用的别名），填真正进程的名字（不带 .exe）
    [string]$ProcessName,
    # 有启动画面的程序，用它指定"真正的主窗口"标题应匹配的正则
    [string]$TitleRegex = '.',
    [int]$Runs = 5,
    [int]$SettleSeconds = 5,
    [int]$TimeoutSeconds = 60,
    [string]$Out
)

$ErrorActionPreference = 'Stop'
$rows = @()

for ($run = 1; $run -le $Runs; $run++) {
    $before = @()
    if ($ProcessName) { $before = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | ForEach-Object { $_.Id }) }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $launched = if ($ExeArgs.Count) { Start-Process -FilePath $Exe -ArgumentList $ExeArgs -PassThru } else { Start-Process -FilePath $Exe -PassThru }

    $proc = $null; $tWindow = $null; $title = ''
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        if ($ProcessName) {
            $cands = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Where-Object { $before -notcontains $_.Id })
        } else {
            $launched.Refresh(); $cands = @($launched)
        }
        foreach ($c in $cands) {
            if (-not $c.HasExited -and $c.MainWindowHandle -ne 0 -and $c.MainWindowTitle -match $TitleRegex) {
                $proc = $c; $tWindow = $sw.Elapsed.TotalMilliseconds; $title = $c.MainWindowTitle; break
            }
        }
        if ($proc) { break }
        Start-Sleep -Milliseconds 5
    }
    if (-not $proc) {
        Write-Warning "第 $run 轮：$TimeoutSeconds 秒内没等到主窗口（程序可能把启动交给了已在运行的实例）"
        continue
    }

    # 控制台子系统的程序没有"输入空闲"的概念，这一项留空。
    $tIdle = $null
    try { $null = $proc.WaitForInputIdle(30000); $tIdle = [math]::Round($sw.Elapsed.TotalMilliseconds, 0) } catch { }

    Start-Sleep -Seconds $SettleSeconds
    $proc.Refresh()
    $rows += [pscustomobject]@{
        run            = $run
        exe            = [IO.Path]::GetFileName($proc.Path)
        window_ms      = [math]::Round($tWindow, 0)
        input_idle_ms  = $tIdle
        working_set_mb = [math]::Round($proc.WorkingSet64 / 1MB, 1)
        private_mb     = [math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
        threads        = $proc.Threads.Count
        handles        = $proc.HandleCount
        title          = $title
    }

    $null = $proc.CloseMainWindow()
    if (-not $proc.WaitForExit(8000)) {
        # 不强杀：可能弹了确认框，交给人处理。
        Write-Warning "第 $run 轮：进程 $($proc.Id) 没有在 8 秒内退出，已停止后续轮次，请手动关闭它"
        break
    }
    Start-Sleep -Seconds 1
}

$rows | Format-Table -AutoSize
if ($Out -and $rows.Count) {
    $dir = Split-Path -Parent $Out
    if ($dir) { $null = New-Item -ItemType Directory -Force $dir }
    $rows | Export-Csv -Path $Out -NoTypeInformation -Encoding UTF8
    "saved $Out"
}
