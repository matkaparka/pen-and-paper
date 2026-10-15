# Fill a common file dialog of a given process with window messages (no pointer, no keyboard focus needed).
# Prints the dialog title, the preset file name and the selected file type, then either sets -Path and
# presses OK, or cancels (-Cancel). -Replace answers Yes to an overwrite prompt. -Kind N selects file type N (1-based).
param(
    [string]$ProcessName = 'zhibi-test',
    [string]$Path = '',
    [switch]$Cancel,
    [switch]$Replace,
    [int]$Kind = 0,
    [int]$TimeoutMs = 8000
)
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class Dlg {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr h, int id);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowEx(IntPtr p, IntPtr after, string cls, string title);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessage(IntPtr h, int msg, IntPtr w, StringBuilder l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessage(IntPtr h, int msg, IntPtr w, string l);
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, int msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, int msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr p, EnumProc f, IntPtr l);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetParent(IntPtr h);
    public static IntPtr OtherCombo(IntPtr dlg, IntPtr not) {
        IntPtr hit = IntPtr.Zero;
        EnumChildWindows(dlg, (h, l) => {
            var c = new StringBuilder(64); GetClassName(h, c, 64);
            if (c.ToString() == "ComboBox" && h != not) { hit = h; return false; }
            return true;
        }, IntPtr.Zero);
        return hit;
    }
    /// Any descendant window of class cls whose own control id (or its parent's) is one of ids.
    public static IntPtr Child(IntPtr dlg, string cls, int[] ids) {
        IntPtr hit = IntPtr.Zero;
        EnumChildWindows(dlg, (h, l) => {
            var c = new StringBuilder(64); GetClassName(h, c, 64);
            if (c.ToString() != cls) return true;
            int own = GetDlgCtrlID(h), up = GetDlgCtrlID(GetParent(h)), upup = GetDlgCtrlID(GetParent(GetParent(h)));
            foreach (var id in ids) { if (own == id || up == id || upup == id) { hit = h; return false; } }
            return true;
        }, IntPtr.Zero);
        return hit;
    }
    public static List<IntPtr> Dialogs(uint pid) {
        var found = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p == pid && IsWindowVisible(h)) {
                var c = new StringBuilder(64); GetClassName(h, c, 64);
                if (c.ToString() == "#32770") found.Add(h);
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    public static string Text(IntPtr h) { var s = new StringBuilder(1024); GetWindowText(h, s, 1024); return s.ToString(); }
    public static string EditText(IntPtr h) { var s = new StringBuilder(1024); SendMessage(h, 0x000D, (IntPtr)1024, s); return s.ToString(); }
}
"@
$proc = Get-Process $ProcessName -ErrorAction Stop | Select-Object -First 1
$deadline = (Get-Date).AddMilliseconds($TimeoutMs)
$dlg = [IntPtr]::Zero
while ($dlg -eq [IntPtr]::Zero -and (Get-Date) -lt $deadline) {
    $list = [Dlg]::Dialogs([uint32]$proc.Id)
    if ($list.Count -gt 0) { $dlg = $list[0] } else { Start-Sleep -Milliseconds 150 }
}
if ($dlg -eq [IntPtr]::Zero) { 'NO DIALOG'; exit 2 }
Start-Sleep -Milliseconds 500
"title: " + [Dlg]::Text($dlg)

if ($Cancel) {
    [void][Dlg]::PostMessage($dlg, 0x0111, [IntPtr]2, [IntPtr]::Zero)
}
# File name: an Edit under combo box 1148 (open dialogs) or 1001 (save dialogs, nested deeper).
$edit = [Dlg]::Child($dlg, 'Edit', @(1148, 1001))
if ($edit -eq [IntPtr]::Zero) { 'NO NAME BOX'; exit 3 }
"name: " + [Dlg]::EditText($edit)

# File type: combo box 1136 in open dialogs; in save dialogs it has no id, so take the combo box that does not
# hold the file name. CB_GETCURSEL = 0x147, CB_GETLBTEXT = 0x148, CB_SETCURSEL = 0x14E.
$type = [Dlg]::Child($dlg, 'ComboBox', @(1136))
if ($type -eq [IntPtr]::Zero) { $type = [Dlg]::OtherCombo($dlg, [Dlg]::GetParent($edit)) }
if ($type -ne [IntPtr]::Zero) {
    if ($Kind -gt 0) {
        [void][Dlg]::SendMessage($type, 0x14E, [IntPtr]($Kind - 1), [IntPtr]::Zero)
        # Tell the dialog the selection changed: WM_COMMAND (CBN_SELCHANGE << 16 | 1136).
        [void][Dlg]::SendMessage([Dlg]::GetParent($type), 0x0111, [IntPtr]((1 -shl 16) -bor 1136), $type)
    }
    $i = [int][Dlg]::SendMessage($type, 0x147, [IntPtr]::Zero, [IntPtr]::Zero)
    $s = New-Object System.Text.StringBuilder 512
    [void][Dlg]::SendMessage($type, 0x148, [IntPtr]$i, $s)
    "type: " + ($i + 1) + " " + $s.ToString()
}

if ($Cancel) {
    'cancelled'
    exit 0
}
[void][Dlg]::SendMessage($edit, 0x000C, [IntPtr]::Zero, $Path)
[void][Dlg]::PostMessage($dlg, 0x0111, [IntPtr]1, [IntPtr]::Zero)
'pressed ok'
if ($Replace) {
    $deadline = (Get-Date).AddMilliseconds(4000)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
        foreach ($h in [Dlg]::Dialogs([uint32]$proc.Id)) {
            if ($h -ne $dlg) {
                "prompt: " + [Dlg]::Text($h)
                # TDM_CLICK_BUTTON (WM_USER + 102) with IDYES for a task dialog; WM_COMMAND IDYES for a message box.
                [void][Dlg]::SendMessage($h, 0x0400 + 102, [IntPtr]6, [IntPtr]::Zero)
                'answered yes'
                exit 0
            }
        }
    }
    'NO REPLACE PROMPT'
}
