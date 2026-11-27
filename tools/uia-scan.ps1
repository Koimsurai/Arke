# Starts a built Arke (target\debug\arke.exe) with a throwaway profile and checks what UI Automation
# sees of it, the way a screen reader or the keyboard does: controls without names, buttons that
# cannot be invoked, focusable elements that are not in the tree, and a Tab walk that must move
# focus and come back round (no keyboard trap). Used by uia-scan.yml.
#   -CheckDesktop       only checks that the runner has an interactive desktop that can show a window
#   -Page folder|home   which page to scan
#   -Enforce            exit 1 when a check fails (without it the findings are reported only)
[CmdletBinding(PositionalBinding = $false)]
param(
  [ValidateSet('folder', 'home')] [string] $Page = 'folder',
  [ValidateRange(1, 500)] [int] $Tabs = 60,
  [switch] $CheckDesktop,
  [switch] $Enforce
)
$ErrorActionPreference = 'Stop'
$out = Join-Path $PWD "uia-scan\$Page"
if ($CheckDesktop) { $out = Join-Path $PWD 'uia-scan\desktop' }
New-Item -ItemType Directory -Force $out | Out-Null

function Say($m) { Write-Host $m; Add-Content "$out\log.txt" $m }
function Sum($m) { if ($env:GITHUB_STEP_SUMMARY) { Add-Content $env:GITHUB_STEP_SUMMARY $m } }
function Cell($s) { ([string]$s) -replace '\|', '/' -replace '[\r\n]+', ' ' }

Add-Type -AssemblyName System.Drawing, UIAutomationClient, UIAutomationTypes
Add-Type @'
using System; using System.Runtime.InteropServices; using System.Text; using System.Threading;
public static class W {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
  [DllImport("user32.dll")] public static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
  [DllImport("user32.dll")] public static extern bool CloseDesktop(IntPtr d);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint f, IntPtr e);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [StructLayout(LayoutKind.Sequential)] public struct R { public int l, t, r, b; }
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out R r);
  delegate bool Each(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(Each f, IntPtr l);
  static string Class(IntPtr h) { var s = new StringBuilder(256); GetClassName(h, s, 256); return s.ToString(); }
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
  // SetForegroundWindow is refused unless the process just had input: a tap on Alt counts.
  public static void Front(IntPtr h) {
    keybd_event(0x12, 0, 0, IntPtr.Zero); keybd_event(0x12, 0, 2, IntPtr.Zero);
    SetForegroundWindow(h);
  }
  public static void Tab() {
    keybd_event(0x09, 0, 0, IntPtr.Zero); Thread.Sleep(30); keybd_event(0x09, 0, 2, IntPtr.Zero);
  }
}
'@
[W]::SetProcessDPIAware() | Out-Null

function Fail-Desktop($why) {
  Write-Host "::error title=No interactive desktop::$why"
  Sum "## UI Automation scan: no interactive desktop`n`n$why`n"
  exit 1
}

# ---- 1. Can this runner show a window at all? ----
if ($CheckDesktop) {
  $width = [W]::GetSystemMetrics(0); $height = [W]::GetSystemMetrics(1)
  Say "UserInteractive=$([Environment]::UserInteractive), screen ${width}x$height"
  if (-not [Environment]::UserInteractive) { Fail-Desktop 'The job runs in a non-interactive session (service), so no window can be shown. Use a runner that is logged in to a desktop.' }
  $desk = [W]::OpenInputDesktop(0, $false, 1)
  if ($desk -eq [IntPtr]::Zero) { Fail-Desktop 'OpenInputDesktop failed: this session is not attached to the input desktop (locked, disconnected or no desktop).' }
  [W]::CloseDesktop($desk) | Out-Null
  if ($width -le 0 -or $height -le 0) { Fail-Desktop "The screen reports ${width}x$height." }
  $np = Start-Process notepad -PassThru
  $h = [IntPtr]::Zero
  for ($i = 0; $i -lt 40 -and $h -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 500; $h = [W]::WindowOf([uint32]$np.Id) }
  Stop-Process -Id $np.Id -Force -ErrorAction SilentlyContinue
  if ($h -eq [IntPtr]::Zero) { Fail-Desktop 'Started Notepad, but no visible window appeared within 20 s.' }
  Say "Desktop OK: a Notepad window opened on a ${width}x$height screen."
  exit 0
}

# ---- 2. Start Arke on a throwaway profile ----
$profileDir = Join-Path $env:RUNNER_TEMP "arke-profile-$Page"
Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$profileDir\Roaming\Arke", "$profileDir\Local" | Out-Null
$settings = @{ whats_new_auto = $false; keep_running = $false; update_auto = $false; open_folders_in_arke = $false
  welcome_seen = $true; theme = 'light'; start_location = 'home_page'
  window = @{ x = 60; y = 60; width = 1100; height = 700; maximized = $false } }
[IO.File]::WriteAllText("$profileDir\Roaming\Arke\settings.json", ($settings | ConvertTo-Json -Depth 4))
$env:APPDATA = "$profileDir\Roaming"; $env:LOCALAPPDATA = "$profileDir\Local"
$env:ARKE_NO_FOLDER_HANDLER = '1'; $env:ARKE_UNINSTALL_SANDBOX = '1'; $env:ARKE_NO_TRAY = '1'

# A folder with a few files and subfolders, for the folder page. Home starts without a path.
$fixture = Join-Path $env:RUNNER_TEMP 'uia-fixture'
Remove-Item $fixture -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$fixture\alpha", "$fixture\beta", "$fixture\gamma" | Out-Null
1..12 | ForEach-Object { Set-Content "$fixture\file-$_.txt" "file $_" }

$exe = Join-Path $PWD 'target\debug\arke.exe'
$proc = if ($Page -eq 'folder') { Start-Process $exe $fixture -PassThru } else { Start-Process $exe -PassThru }
$window = [IntPtr]::Zero
for ($i = 0; $i -lt 120 -and $window -eq [IntPtr]::Zero; $i++) {
  Start-Sleep -Milliseconds 500; $window = [W]::WindowOf([uint32]$proc.Id)
}
if ($window -eq [IntPtr]::Zero) {
  Write-Host '::error title=Arke window::Arke started, but no window appeared within 60 s.'
  Sum "## UI Automation scan ($Page): Arke showed no window within 60 s`n"
  Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
  exit 1
}
Start-Sleep 3
Say "consoles minimized: $([W]::HideConsoles())"
[W]::Front($window); Start-Sleep 1
$arkePid = $proc.Id

$AE = [System.Windows.Automation.AutomationElement]
$root = $AE::FromHandle($window)
$descendants = [System.Windows.Automation.TreeScope]::Descendants
$anything = [System.Windows.Automation.Condition]::TrueCondition

# Waits until the tree stops growing, so the page is loaded before it is scanned.
$count = -1; $stable = 0
for ($i = 0; $i -lt 30 -and $stable -lt 3; $i++) {
  $n = $root.FindAll($descendants, $anything).Count
  if ($n -eq $count) { $stable++ } else { $stable = 0; $count = $n }
  Start-Sleep -Seconds 1
}
Say "tree settled at $count elements"

try {
  $b = New-Object Drawing.Bitmap 1280, 900; $g = [Drawing.Graphics]::FromImage($b)
  $g.CopyFromScreen(0, 0, 0, 0, $b.Size); $b.Save("$out\screen.png"); $g.Dispose(); $b.Dispose()
} catch { Say "screenshot failed: $_" }

# ---- 3. Scan the tree ----
$findings = New-Object Collections.Generic.List[object]
function Add-Finding($rule, $e, $note) {
  $findings.Add([pscustomobject]@{ Rule = $rule; Type = $e.Type; Name = $e.Name; Class = $e.Class; Id = $e.Id; Note = $note })
}
$needName = 'Button', 'CheckBox', 'RadioButton', 'ComboBox', 'Edit', 'Hyperlink', 'ListItem', 'MenuItem',
  'SplitButton', 'TabItem', 'TreeItem', 'Slider', 'Spinner', 'DataItem', 'Tab'
$needInvoke = 'Button', 'SplitButton', 'Hyperlink', 'MenuItem'
$actionPatterns = 'Invoke|Toggle|ExpandCollapse|SelectionItem'

$all = $root.FindAll($descendants, $anything)
$known = @{}      # runtime id -> $true, for the "focused but not in the tree" check
$types = @{}
$elements = 0
foreach ($el in $all) {
  try {
    $c = $el.Current
    $info = [pscustomobject]@{
      Type = ($c.ControlType.ProgrammaticName -replace '^ControlType\.', ''); Name = $c.Name
      Class = $c.ClassName; Id = $c.AutomationId
    }
    $known[($el.GetRuntimeId() -join '.')] = $true
    $elements++
    $types[$info.Type] = 1 + [int]$types[$info.Type]
    if ($c.IsOffscreen) { continue }
    $named = ($c.Name -and $c.Name.Trim()) -or $c.LabeledBy
    if ($needName -contains $info.Type -and -not $named) { Add-Finding 'unnamed-control' $info 'a control a user can act on has no Name' }
    elseif ($c.IsKeyboardFocusable -and -not $named -and $info.Type -ne 'Window') { Add-Finding 'unnamed-focusable' $info 'focusable, but has no Name' }
    if ($c.IsKeyboardFocusable -and -not $c.IsControlElement) { Add-Finding 'focusable-not-control' $info 'focusable, but hidden from the control view of the tree' }
    if ($needInvoke -contains $info.Type -and $c.IsEnabled) {
      $patterns = ($el.GetSupportedPatterns() | ForEach-Object { $_.ProgrammaticName }) -join ' '
      if ($patterns -notmatch $actionPatterns) { Add-Finding 'no-invoke' $info "no Invoke/Toggle/ExpandCollapse/SelectionItem pattern (has: $patterns)" }
    }
  } catch { }  # the element went away while it was read
}
Say "scanned $elements elements: $(($types.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ' ')"

# ---- 4. Tab walk ----
function Focus-Now {
  try {
    $e = $AE::FocusedElement
    if (-not $e) { return $null }
    $c = $e.Current
    [pscustomobject]@{
      Key = ($e.GetRuntimeId() -join '.'); Pid = $c.ProcessId
      Type = ($c.ControlType.ProgrammaticName -replace '^ControlType\.', ''); Name = $c.Name; Class = $c.ClassName; Id = $c.AutomationId
    }
  } catch { $null }
}
function Label($f) { if (-not $f) { '(none)' } else { "$($f.Type) '$($f.Name)' [$($f.Class)]" } }

[W]::Front($window); Start-Sleep -Seconds 1
if ([W]::GetForegroundWindow() -ne $window) { Say 'warning: Arke is not the foreground window; Tab may go elsewhere' }
$start = Focus-Now
Say "start focus: $(Label $start)"
$seq = New-Object Collections.Generic.List[object]
$walkFail = New-Object Collections.Generic.List[string]
$firstKey = if ($start) { $start.Key } else { $null }
Say "Tab walk: up to $Tabs presses"
$harnessFail = $false
$prev = $start; $stuck = 0; $maxStuck = 0; $cycle = 0; $moves = 0
for ($n = 1; $n -le $Tabs; $n++) {
  [W]::Tab()
  $f = $null
  for ($w = 0; $w -lt 8; $w++) {    # up to ~2 s for focus to settle on something new
    Start-Sleep -Milliseconds 250
    $f = Focus-Now
    if ($f -and (-not $prev -or $f.Key -ne $prev.Key)) { break }
  }
  $seq.Add([pscustomobject]@{ N = $n; Focus = $f })
  Say "  Tab ${n}: $(Label $f)"
  if (-not $f) {
    $walkFail.Add("Tab ${n}: UI Automation reports no focused element")
    $stuck++; $maxStuck = [Math]::Max($maxStuck, $stuck); continue
  }
  if ($f.Pid -ne $arkePid) { $walkFail.Add("Tab ${n}: focus left Arke (process $($f.Pid): $(Label $f))"); break }
  if (-not $known.ContainsKey($f.Key)) {
    # Some elements only exist while focus is on them (the address bar's edit box, the search
    # options): read the tree again now. Found there is fine; still missing is a real finding.
    foreach ($el in $root.FindAll($descendants, $anything)) {
      try { $known[($el.GetRuntimeId() -join '.')] = $true } catch { }
    }
    if ($known.ContainsKey($f.Key)) { Say "    (appeared on focus: $(Label $f))" }
    else { Add-Finding 'focused-not-in-tree' $f "Tab $n focused an element that is not in the tree, even when read again" }
  }
  if (-not ($f.Name -and $f.Name.Trim())) { Add-Finding 'focused-unnamed' $f "Tab $n focused an element with no Name" }
  if ($prev -and $f.Key -eq $prev.Key) { $stuck++; $maxStuck = [Math]::Max($maxStuck, $stuck) } else { $stuck = 0; $moves++ }
  if (-not $firstKey) { $firstKey = $f.Key }
  elseif ($f.Key -eq $firstKey -and $moves -ge 2) { $cycle = $n; break }
  $prev = $f
}
$distinct = @($seq | Where-Object { $_.Focus } | ForEach-Object { $_.Focus.Key } | Select-Object -Unique).Count
if ($seq.Count -eq 0) { $walkFail.Add('no Tab press was sent (the press loop did not run)'); $harnessFail = $true }
if ($moves -lt 2 -or $distinct -lt 3) { $walkFail.Add("focus did not move: $moves moves, $distinct distinct elements in $($seq.Count) presses") }
if ($maxStuck -ge 3) { $walkFail.Add("keyboard trap: focus stayed on one element (or on nothing) for $maxStuck Tab presses in a row") }
if (-not $cycle -and $walkFail.Count -eq 0) { $walkFail.Add("focus never came back to the start element within $Tabs presses (no cycle)") }

# ---- 5. Report ----
Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Copy-Item "$profileDir\Local\Arke\arke.log" $out -ErrorAction SilentlyContinue

$grouped = @($findings | Group-Object Rule, Type, Name, Class | ForEach-Object {
  $f = $_.Group[0]
  [pscustomobject]@{ Rule = $f.Rule; Type = $f.Type; Name = $f.Name; Class = $f.Class; Id = $f.Id; Note = $f.Note; Count = $_.Count }
})
$title = if ($Page -eq 'home') { 'Home' } else { 'Folder page' }
Sum "## UI Automation scan: $title`n"
$cycleText = if ($cycle) { "cycle closed after $cycle presses" } else { 'no cycle' }
Sum "$elements elements in the tree. Tab walk: $($seq.Count) presses, $distinct distinct elements, $cycleText.`n"
if ($walkFail.Count) {
  Sum "**Tab walk failed**`n"
  foreach ($m in $walkFail) { Sum "- $m"; Write-Host "::error title=Tab walk ($Page)::$m" }
  Sum ''
} else { Sum "Tab walk passed.`n" }
if ($grouped.Count) {
  Sum "**$($grouped.Count) kinds of finding** ($($findings.Count) elements)`n"
  Sum '| Rule | Type | Name | Class | AutomationId | Count |'; Sum '| --- | --- | --- | --- | --- | --- |'
  $grouped | Sort-Object Rule, Type, Name | Select-Object -First 80 | ForEach-Object {
    Sum "| $($_.Rule) | $($_.Type) | $(Cell $_.Name) | $(Cell $_.Class) | $(Cell $_.Id) | $($_.Count) |"
  }
  if ($grouped.Count -gt 80) { Sum "`n...and $($grouped.Count - 80) more in the findings.csv artifact." }
  Sum ''
  foreach ($g in $grouped) {
    Write-Host "::warning title=UIA $($g.Rule) ($Page)::$($g.Type) '$($g.Name)' [$($g.Class)] x$($g.Count): $($g.Note)"
  }
} else { Sum "No tree findings.`n" }
Sum '<details><summary>Tab order</summary>'; Sum ''
Sum '| Press | Focus |'; Sum '| --- | --- |'
Sum "| start | $(Cell (Label $start)) |"
foreach ($s in $seq) { Sum "| $($s.N) | $(Cell (Label $s.Focus)) |" }
Sum ''; Sum '</details>'; Sum ''
if ($grouped.Count) { $grouped | Export-Csv "$out\findings.csv" -NoTypeInformation }

$failed = $walkFail.Count -gt 0 -or $grouped.Count -gt 0
# Report-only runs succeed with findings; a harness failure (nothing was tested) always fails.
if ($harnessFail) { Say 'FAILED: the scan could not run'; exit 1 }
if ($failed -and $Enforce) { Say 'FAILED'; exit 1 }
Say $(if ($failed) { 'findings reported (not enforced)' } else { 'clean' })
exit 0
