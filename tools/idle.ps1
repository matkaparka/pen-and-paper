# SPDX-License-Identifier: GPL-3.0-or-later
# 等到用户有一阵没碰键盘鼠标再继续，用在会抢前台或接管指针的测试之前。
# 用法：powershell -File idle.ps1 -Seconds 20 -Timeout 120
# 等到了退出码为 0，超时退出码为 1。每次都打印当前的空闲秒数和前台窗口标题。
param(
    [int]$Seconds = 20,
    [int]$Timeout = 120
)

Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class IdleProbe {
    [StructLayout(LayoutKind.Sequential)] struct LASTINPUTINFO { public uint cbSize; public uint dwTime; }
    [DllImport("user32.dll")] static extern bool GetLastInputInfo(ref LASTINPUTINFO p);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
    public static uint Seconds() {
        var i = new LASTINPUTINFO(); i.cbSize = (uint)Marshal.SizeOf(i); GetLastInputInfo(ref i);
        return ((uint)Environment.TickCount - i.dwTime) / 1000;
    }
    public static string Front() { var s = new System.Text.StringBuilder(256); GetWindowText(GetForegroundWindow(), s, 256); return s.ToString(); }
}
'@

$waited = 0
while ($true) {
    $idle = [IdleProbe]::Seconds()
    if ($idle -ge $Seconds) {
        Write-Output ("idle {0}s, foreground: {1}" -f $idle, [IdleProbe]::Front())
        exit 0
    }
    if ($waited -ge $Timeout) {
        Write-Output ("busy: idle only {0}s after waiting {1}s, foreground: {2}" -f $idle, $waited, [IdleProbe]::Front())
        exit 1
    }
    Start-Sleep -Seconds 2
    $waited += 2
}
