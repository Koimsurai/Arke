# Runs the base and the PR build of arke.exe against each other on this machine and writes the
# comparison to the job summary. Called by .github/workflows/perf.yml.
#
#   bench.ps1 -Base <exe> -Head <exe> [-Runs 5] [-Fail]
#
# - list benchmark (`arke <folder> --bench list`, headless): listing + sorting 100,000 items, and
#   the process's peak working set.
# - startup benchmark (`arke <folder> --bench startup`, opens a window): first frame and first
#   full listing of the same folder, and the peak working set sampled while it ran. A runner
#   without a usable display or GPU can't open it; that is reported, not failed.
# - The two builds run alternately (the order flips every round) so drift on the runner hits both
#   alike, after one discarded warm-up run each. The medians are compared.
# - A median that is worse than the base by more than the threshold (and by more than a minimum
#   amount, so noise on small numbers is ignored) is a warning annotation. With -Fail it fails the
#   job instead.
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Base,
    [Parameter(Mandatory)] [string] $Head,
    [int] $Runs = 5,
    [switch] $Fail,
    # Relative worsening that counts, in percent
    [double] $TimePercent = 20,
    [double] $MemoryPercent = 15,
    # ... and the least absolute worsening that counts
    [double] $MinMs = 10,
    [double] $MinMegabytes = 8
)

$ErrorActionPreference = 'Stop'
$temp = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$folder = Join-Path $temp 'perf-100k'
$summary = if ($env:GITHUB_STEP_SUMMARY) { $env:GITHUB_STEP_SUMMARY } else { Join-Path $temp 'summary.md' }
$env:ARKE_NO_FOLDER_HANDLER = '1'
$regressions = [System.Collections.Generic.List[string]]::new()

function Read-Report([string] $path) {
    $values = @{}
    if (Test-Path $path) {
        foreach ($line in Get-Content $path) {
            if ($line -match '^([a-z_]+): (-?[0-9.]+)$') { $values[$Matches[1]] = [double] $Matches[2] }
        }
    }
    return $values
}

function Get-Median([double[]] $samples) {
    if (-not $samples -or $samples.Count -eq 0) { return $null }
    $sorted = $samples | Sort-Object
    $mid = [int][math]::Floor($sorted.Count / 2)
    if ($sorted.Count % 2) { return $sorted[$mid] }
    return ($sorted[$mid - 1] + $sorted[$mid]) / 2
}

# `arke <folder> --bench list`; a build that doesn't know the mode starts the app instead, which is
# stopped after 30 s and counts as "no report".
function Invoke-List([string] $exe) {
    $out = Join-Path $temp ("list-" + [guid]::NewGuid() + '.txt')
    $p = Start-Process -FilePath $exe -ArgumentList @($folder, '--bench', 'list', '--bench-out', $out) -PassThru -WindowStyle Hidden
    if (-not $p.WaitForExit(120000)) { $p.Kill() }
    $r = Read-Report $out
    Remove-Item $out -ErrorAction SilentlyContinue
    # A build without the mode opens a window benchmark instead; its report has no `items`.
    if (-not $r.ContainsKey('items')) { return @{} }
    if ($r.ContainsKey('peak_working_set_bytes')) { $r['peak_mb'] = $r['peak_working_set_bytes'] / 1MB }
    return $r
}

function Invoke-Window([string] $exe) {
    $out = Join-Path $temp ("win-" + [guid]::NewGuid() + '.txt')
    $r = @{}
    try {
        $p = Start-Process -FilePath $exe -ArgumentList @($folder, '--bench', 'startup', '--bench-out', $out, '--no-resident') -PassThru
        $peak = 0
        $clock = [Diagnostics.Stopwatch]::StartNew()
        while (-not $p.WaitForExit(30)) {
            try { $p.Refresh(); $peak = [math]::Max($peak, $p.PeakWorkingSet64) } catch { }
            if ($clock.Elapsed.TotalSeconds -gt 60) { $p.Kill(); break }
        }
        $r = Read-Report $out
        if ($r.ContainsKey('first_listing_ms') -and $peak -gt 0) { $r['peak_mb'] = $peak / 1MB }
    } catch {
        Write-Host "window benchmark did not run: $_"
    }
    Remove-Item $out -ErrorAction SilentlyContinue
    return $r
}

# Warm-up, then $Runs rounds with the order flipping each round. Returns @{ base = @{metric=[samples]}; head = ... }.
function Measure-Pair([scriptblock] $run) {
    $samples = @{ base = @{}; head = @{} }
    $null = & $run $Base
    $null = & $run $Head
    for ($i = 0; $i -lt $Runs; $i++) {
        $order = if ($i % 2 -eq 0) { @('base', 'head') } else { @('head', 'base') }
        foreach ($side in $order) {
            $exe = if ($side -eq 'base') { $Base } else { $Head }
            $r = & $run $exe
            foreach ($key in $r.Keys) {
                if (-not $samples[$side].ContainsKey($key)) { $samples[$side][$key] = [System.Collections.Generic.List[double]]::new() }
                $samples[$side][$key].Add($r[$key])
            }
        }
    }
    return $samples
}

function Add-Table([string] $title, $samples, $rows) {
    $lines = @("### $title", '', '| metric | base | PR | change |', '| --- | ---: | ---: | ---: |')
    foreach ($row in $rows) {
        $key, $label, $unit = $row
        $b = if ($samples.base.ContainsKey($key)) { Get-Median $samples.base[$key] } else { $null }
        $h = if ($samples.head.ContainsKey($key)) { Get-Median $samples.head[$key] } else { $null }
        if ($null -eq $h) { $lines += "| $label | n/a | n/a | not measured |"; continue }
        if ($null -eq $b) { $lines += ('| {0} | n/a | {1:N1} {2} | no base number |' -f $label, $h, $unit); continue }
        $delta = $h - $b
        $percent = if ($b -gt 0) { 100 * $delta / $b } else { 0 }
        $isMemory = $unit -eq 'MB'
        $limit = if ($isMemory) { $MemoryPercent } else { $TimePercent }
        $least = if ($isMemory) { $MinMegabytes } else { $MinMs }
        $worse = ($percent -gt $limit) -and ($delta -gt $least)
        $mark = if ($worse) { ' :warning:' } else { '' }
        $lines += ('| {0} | {1:N1} {3} | {2:N1} {3} | {4:+0.0;-0.0}% ({5:+0.0;-0.0} {3}){6} |' -f $label, $b, $h, $unit, $percent, $delta, $mark)
        if ($worse) {
            $regressions.Add(('{0}: {1:N1} {2} -> {3:N1} {2} ({4:+0.0}%)' -f $title, $b, $unit, $h, $percent))
        }
    }
    $lines += ''
    Add-Content -Path $summary -Value $lines
}

Add-Content -Path $summary -Value @('## Performance', '', ('Medians of {0} runs per build on the same runner, base and PR alternating. Warns above +{1}% time / +{2}% memory.' -f $Runs, $TimePercent, $MemoryPercent), '')

# The folder: made once by the PR build (the base may not have the mode), then listed by both.
$made = Invoke-List $Head
if (-not $made.ContainsKey('items') -or $made['items'] -lt 100000) {
    throw "the PR build's --bench list did not produce a 100,000 item folder: $($made | Out-String)"
}

$list = Measure-Pair { param($exe) Invoke-List $exe }
if ($list.base.Count -eq 0) {
    Add-Content -Path $summary -Value @('> The base build has no `--bench list` yet, so only the PR numbers are shown. Comparison starts once it is on main.', '')
}
Add-Table '100k folder, headless (list + sort)' $list @(
    , @('list_enumerate_ms', 'list', 'ms')
    , @('list_sort_ms', 'sort', 'ms')
    , @('list_total_ms', 'list + sort', 'ms')
    , @('peak_mb', 'peak working set', 'MB')
)

$window = Measure-Pair { param($exe) Invoke-Window $exe }
if ($window.head.Count -eq 0) {
    Add-Content -Path $summary -Value @('### Startup (window)', '', 'The window benchmark could not run on this runner (no display or GPU?). Not compared.', '')
} else {
    Add-Table 'startup on the 100k folder (window)' $window @(
        , @('first_frame_ms', 'first frame', 'ms')
        , @('first_listing_ms', 'first full listing', 'ms')
        , @('list_total_ms', 'listing in the app (enumerate + sort)', 'ms')
        , @('peak_mb', 'peak working set', 'MB')
    )
}

foreach ($line in $regressions) { Write-Host "::warning title=Performance::$line" }
if ($regressions.Count -gt 0) {
    Add-Content -Path $summary -Value @($(if ($Fail) { '**The gate is on: this fails the job.**' } else { 'Warning only: set the repository variable `PERF_GATE_FAIL` to `true` to fail on this.' }), '')
    if ($Fail) { exit 1 }
}
