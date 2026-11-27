# Starts a built Arke (target\debug\arke.exe) with a throwaway profile and takes screenshots of its
# window with 24 px around it: active, then inactive (Notepad in front, out of the way), plus the
# whole screen. Logs the pixel colours across the four edges, outside to inside, so the top edge
# can be compared with the border DWM draws on the other three. Used by window-shots.yml.
param([ValidateSet('light', 'dark')] [string] $Theme = 'light')
$ErrorActionPreference = 'Stop'
$out = Join-Path $PWD 'shots'
New-Item -ItemType Directory -Force $out | Out-Null
function Log($m) { Write-Host $m; Add-Content "$out\log.txt" $m }
$build = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').CurrentBuild
Log "OS build $build, theme $Theme"

Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices; using System.Text;
public static class W {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [StructLayout(LayoutKind.Sequential)] public struct R { public int l, t, r, b; }
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out R r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int hh, bool p);
  [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr h, int a, out R r, int s);
  delegate bool Each(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(Each f, IntPtr l);
  static string Class(IntPtr h) { var s = new StringBuilder(256); GetClassName(h, s, 256); return s.ToString(); }
  // The window as DWM draws it, border included (DWMWA_EXTENDED_FRAME_BOUNDS).
  public static R Frame(IntPtr h) { R r; DwmGetWindowAttribute(h, 9, out r, 16); return r; }
  // Minimizes every console window: the runner's agent, and a debug build's own console.
  public static int HideConsoles() {
    int n = 0;
    EnumWindows((h, l) => { if (IsWindowVisible(h) && Class(h) == "ConsoleWindowClass") { ShowWindow(h, 6); n++; } return true; }, IntPtr.Zero);
    return n;
  }
  // The largest visible top-level window of process `pid` that is not a console.
  public static IntPtr WindowOf(uint pid) {
    IntPtr best = IntPtr.Zero; long area = 0;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p == pid && IsWindowVisible(h) && Class(h) != "ConsoleWindowClass") {
        R r; GetWindowRect(h, out r); long a = (long)(r.r - r.l) * (r.b - r.t);
        if (a > area) { area = a; best = h; }
      }
      return true;
    }, IntPtr.Zero);
    return best;
  }
}
'@
[W]::SetProcessDPIAware() | Out-Null

# A profile of its own, so nothing of a real one is read or written.
$profileDir = Join-Path $env:RUNNER_TEMP 'arke-profile'
New-Item -ItemType Directory -Force "$profileDir\Roaming\Arke", "$profileDir\Local" | Out-Null
$settings = @{ whats_new_auto = $false; keep_running = $false; update_auto = $false; open_folders_in_arke = $false
  welcome_seen = $true; theme = $Theme; window = @{ x = 120; y = 120; width = 900; height = 560; maximized = $false } }
[IO.File]::WriteAllText("$profileDir\Roaming\Arke\settings.json", ($settings | ConvertTo-Json -Depth 4))
$env:APPDATA = "$profileDir\Roaming"; $env:LOCALAPPDATA = "$profileDir\Local"
$env:ARKE_NO_FOLDER_HANDLER = '1'; $env:ARKE_UNINSTALL_SANDBOX = '1'

$proc = Start-Process (Join-Path $PWD 'target\debug\arke.exe') 'C:\Windows' -PassThru
$window = [IntPtr]::Zero
for ($i = 0; $i -lt 120 -and $window -eq [IntPtr]::Zero; $i++) {
  Start-Sleep -Milliseconds 500; $window = [W]::WindowOf([uint32]$proc.Id)
}
if ($window -eq [IntPtr]::Zero) { Log 'no Arke window'; exit 1 }
Start-Sleep 6
Log "consoles minimized: $([W]::HideConsoles())"
Start-Sleep 1

function Hex($c) { '{0:X2}{1:X2}{2:X2}' -f $c.R, $c.G, $c.B }
function Shot($name) {
  $f = [W]::Frame($window); $pad = 24
  $width = $f.r - $f.l + 2 * $pad; $height = $f.b - $f.t + 2 * $pad
  $bmp = New-Object Drawing.Bitmap $width, $height; $g = [Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($f.l - $pad, $f.t - $pad, 0, 0, $bmp.Size); $bmp.Save("$out\$name.png")
  $midX = [int]($width / 2); $midY = [int]($height / 2); $span = ($pad - 2)..($pad + 3)
  Log "$name frame $($f.l),$($f.t)-$($f.r),$($f.b)"
  Log ('  top    ' + (($span | ForEach-Object { Hex $bmp.GetPixel($midX, $_) }) -join ' '))
  Log ('  left   ' + (($span | ForEach-Object { Hex $bmp.GetPixel($_, $midY) }) -join ' '))
  Log ('  right  ' + (($span | ForEach-Object { Hex $bmp.GetPixel($width - 1 - $_, $midY) }) -join ' '))
  Log ('  bottom ' + (($span | ForEach-Object { Hex $bmp.GetPixel($midX, $height - 1 - $_) }) -join ' '))
  $g.Dispose(); $bmp.Dispose()
}

[W]::SetForegroundWindow($window) | Out-Null; Start-Sleep 2
Shot 'active'
$notepad = Start-Process notepad -PassThru; Start-Sleep 3; $notepad.Refresh()
[W]::MoveWindow($notepad.MainWindowHandle, 1100, 650, 300, 200, $true) | Out-Null
[W]::SetForegroundWindow($notepad.MainWindowHandle) | Out-Null; Start-Sleep 2
Shot 'inactive'
$screen = New-Object Drawing.Bitmap 1280, 900; $g = [Drawing.Graphics]::FromImage($screen)
$g.CopyFromScreen(0, 0, 0, 0, $screen.Size); $screen.Save("$out\screen.png"); $g.Dispose(); $screen.Dispose()
Stop-Process -Id $notepad.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Copy-Item "$profileDir\Local\Arke\arke.log" $out -ErrorAction SilentlyContinue
Log 'done'
