# TaskBar Plus — 端到端冒烟测试
# 启动 release 版，验证进程存活、dock 窗口锚定、置顶属性、内存占用。
# 运行: powershell -ExecutionPolicy Bypass -File tests/smoke.ps1

$ErrorActionPreference = 'Continue'
$exe = Join-Path $PSScriptRoot '..\src-tauri\target\release\taskbar-plus.exe'
$exe = (Resolve-Path $exe).Path

# 清理旧实例
Get-Process -Name 'taskbar-plus' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep 1

$failures = @()
function Assert-True($cond, $msg) {
  if ($cond) { Write-Host "  PASS  $msg" -ForegroundColor Green }
  else { $script:failures += $msg; Write-Host "  FAIL  $msg" -ForegroundColor Red }
}

Write-Host "== 冒烟测试: 启动 $exe ==" -ForegroundColor Cyan
$proc = Start-Process -FilePath $exe -PassThru
Start-Sleep 10

# 1. 进程存活
$alive = -not $proc.HasExited
Assert-True $alive "进程启动后 10s 仍存活 (pid=$($proc.Id))"

Add-Type -AssemblyName System.Windows.Forms
$screen = [System.Windows.Forms.SystemInformation]::PrimaryMonitorSize

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class Smoke {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
  public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int i);
  public struct RECT { public int Left, Top, Right, Bottom; }
  public static IntPtr DockH; public static IntPtr SettingsH;
  public static RECT DockR; public static bool DockVisible;
  public static void Find(uint pid) {
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p == pid) {
        var sb = new StringBuilder(256); GetWindowText(h, sb, 256);
        if (sb.ToString() == "TaskBar Plus") { DockH = h; GetWindowRect(h, out DockR); DockVisible = IsWindowVisible(h); }
      }
      return true;
    }, IntPtr.Zero);
  }
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
}
"@

[Smoke]::Find([uint32]$proc.Id)

# 2. dock 窗口存在且可见
Assert-True ([Smoke]::DockH -ne [IntPtr]::Zero) "dock 窗口已创建"
Assert-True ([Smoke]::DockVisible) "dock 窗口可见"

$r = [Smoke]::DockR
$w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
Write-Host "  INFO  dock 矩形: $($r.Left),$($r.Top) ${w}x${h}"

# 3. 锚定正确：位于任务栏高度带内（底部 40-64px），右侧（托盘前）
$taskbarTop = $screen.Height - 64
Assert-True ($r.Top -ge $taskbarTop - 200 -and $r.Top -le $screen.Height) "dock 纵向位于屏幕底部任务栏区域 (top=$($r.Top))"
Assert-True ($h -ge 40 -and $h -le 64) "dock 高度符合任务栏高度 (h=$h)"
Assert-True ($r.Right -le $screen.Width -and $r.Right -gt ($screen.Width / 2)) "dock 停靠屏幕右半部 (right=$($r.Right))"

# 4. 置顶属性
$ex = [Smoke]::GetWindowLong([Smoke]::DockH, -20)
Assert-True (($ex -band 0x8) -ne 0) "dock 具有 WS_EX_TOPMOST 置顶属性"

# 5. 内存占用（主进程）
$mem = [math]::Round($proc.WorkingSet64 / 1MB, 1)
Assert-True ($mem -lt 120) "主进程内存占用合理 (${mem} MB < 120 MB)"

# 6. 稳定运行再观察 6s（锚定循环/事件循环不崩溃）
Start-Sleep 6
$alive2 = -not $proc.HasExited
Assert-True $alive2 "持续运行 16s 无崩溃"

# 结束
$proc.Kill()
Write-Host ""
if ($failures.Count -eq 0) { Write-Host "✓ 冒烟测试全部通过" -ForegroundColor Green; exit 0 }
else { Write-Host "✗ 冒烟测试 $($failures.Count) 项失败" -ForegroundColor Red; $failures | ForEach-Object { Write-Host "  - $_" }; exit 1 }
