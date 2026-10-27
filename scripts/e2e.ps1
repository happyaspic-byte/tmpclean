#Requires -Version 7
<#
.SYNOPSIS
  Real-surface QA for the release tmpclean.exe on Windows (plan row F3).

.DESCRIPTION
  Seeds $env:RUNNER_TEMP\e2e\Temp (TMP/TEMP point at it) with nested dirs, a
  read-only file, a junction to an out-of-tree dir holding 3 files, a 300+
  char \\?\ path and a file held open (share mode None) by a background
  PowerShell job, runs `tmpclean --targets user-temp --min-age 0 --yes --json`
  and asserts the outcome. Then benchmarks 50,000 files / 5 levels with
  `--io-permits 1` versus the automatic budget; PASS when auto <= 0.7x single.

  Exits non-zero when any assertion fails. -NegativeControl adds one
  assertion that must fail (proves a failure is not reported as success).
#>
param(
    [string]$Exe = 'target/release/tmpclean.exe',
    [int]$BenchFiles = 50000,
    [double]$MaxRatio = 0.7,
    [switch]$SkipBench,
    [switch]$NegativeControl
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$Exe = (Resolve-Path -LiteralPath $Exe).Path
$parent = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$base = Join-Path $parent 'e2e'
$root = Join-Path $base 'Temp'
$outside = Join-Path $base 'outside'
$script:failures = 0
$job = $null
$savedTmp = $env:TMP
$savedTemp = $env:TEMP
$savedPermits = $env:TMPCLEAN_IO_PERMITS

function Assert-That([bool]$Condition, [string]$Message) {
    if ($Condition) {
        Write-Output "PASS: $Message"
    } else {
        Write-Output "FAIL: $Message"
        $script:failures++
    }
}

function Remove-Fixture {
    if (Test-Path -LiteralPath $base) {
        $junction = Join-Path $root 'junction'
        if (Test-Path -LiteralPath $junction) {
            # Delete the link itself; never recurse through it.
            [IO.Directory]::Delete($junction)
        }
        Remove-Item -LiteralPath $base -Recurse -Force
    }
}

function Invoke-Tmpclean([string[]]$Extra) {
    if ($null -eq $Extra) { $Extra = @() }
    $stdout = & $Exe --targets user-temp --min-age 0 --yes --json @Extra
    $code = $LASTEXITCODE
    $text = ($stdout -join "`n")
    try {
        $report = $text | ConvertFrom-Json
    } catch {
        Write-Output "stdout was not JSON: $text"
        $report = $null
    }
    [pscustomobject]@{ Code = $code; Report = $report; Text = $text }
}

function New-BenchTree([int]$Files) {
    # 5 directory levels: 2 x 5 x 5 x 5 x 5 = 1250 leaf dirs.
    $fanout = 2, 5, 5, 5, 5
    $leaves = [Collections.Generic.List[string]]::new()
    $leaves.Add($root)
    foreach ($width in $fanout) {
        $next = [Collections.Generic.List[string]]::new()
        foreach ($dir in $leaves) {
            for ($i = 0; $i -lt $width; $i++) {
                $child = Join-Path $dir "d$i"
                [void][IO.Directory]::CreateDirectory($child)
                $next.Add($child)
            }
        }
        $leaves = $next
    }
    $bytes = [byte[]]::new(64)
    for ($n = 0; $n -lt $Files; $n++) {
        $dir = $leaves[$n % $leaves.Count]
        [IO.File]::WriteAllBytes((Join-Path $dir "f$n.tmp"), $bytes)
    }
}

try {
    Remove-Fixture
    [void][IO.Directory]::CreateDirectory($root)
    [void][IO.Directory]::CreateDirectory($outside)

    # Nested dirs.
    $nested = Join-Path $root 'n1\n2\n3'
    [void][IO.Directory]::CreateDirectory($nested)
    Set-Content -LiteralPath (Join-Path $root 'top.txt') -Value 'top'
    Set-Content -LiteralPath (Join-Path $nested 'deep.txt') -Value 'deep'

    # Read-only file.
    $readonly = Join-Path $root 'ro\readonly.txt'
    [void][IO.Directory]::CreateDirectory((Split-Path $readonly))
    Set-Content -LiteralPath $readonly -Value 'read-only'
    attrib +r $readonly
    if ($LASTEXITCODE -ne 0) { throw "attrib +r failed" }
    Assert-That ((Get-Item -LiteralPath $readonly).IsReadOnly) 'seed: read-only attribute set'

    # Junction to an out-of-tree dir with 3 files.
    $outsideFiles = 1..3 | ForEach-Object { Join-Path $outside "keep$_.txt" }
    $outsideFiles | ForEach-Object { Set-Content -LiteralPath $_ -Value 'keep' }
    $junction = Join-Path $root 'junction'
    [void](New-Item -ItemType Junction -Path $junction -Target $outside)
    Assert-That ((Get-Item -LiteralPath $junction).LinkType -eq 'Junction') 'seed: junction created'

    # \\?\ path longer than 300 characters.
    $longDir = Join-Path $root 'long'
    while ($longDir.Length -lt 300) { $longDir = Join-Path $longDir ('x' * 60) }
    $longFile = "\\?\$longDir\long-path-file.txt"
    [void][IO.Directory]::CreateDirectory("\\?\$longDir")
    [IO.File]::WriteAllText($longFile, 'long')
    Assert-That ($longFile.Length -gt 300 -and [IO.File]::Exists($longFile)) "seed: long path file ($($longFile.Length) chars)"

    # File held open with share mode None by a background PowerShell job.
    $locked = Join-Path $root 'lockdir\locked.bin'
    [void][IO.Directory]::CreateDirectory((Split-Path $locked))
    Set-Content -LiteralPath $locked -Value 'locked'
    $ready = Join-Path $base 'lock-ready'
    $job = Start-Job -ArgumentList $locked, $ready -ScriptBlock {
        param($path, $readyPath)
        $stream = [IO.File]::Open($path, 'Open', 'Read', 'None')
        Set-Content -LiteralPath $readyPath -Value 'ready'
        # Hold the handle until the job is stopped.
        while ($true) { Start-Sleep -Seconds 1 }
        $stream.Dispose()
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(60)
    while (-not (Test-Path -LiteralPath $ready)) {
        if ($job.State -ne 'Running') { throw "lock job ended early: $(Receive-Job $job 2>&1)" }
        if ([DateTime]::UtcNow -gt $deadline) { throw 'lock job did not signal ready within 60 s' }
        Start-Sleep -Milliseconds 100
    }
    $lockHeld = $false
    try { [IO.File]::Open($locked, 'Open', 'Read', 'ReadWrite').Dispose() } catch [IO.IOException] { $lockHeld = $true }
    Assert-That $lockHeld 'seed: locked file is held open by the background job'

    $env:TMP = $root
    $env:TEMP = $root
    Remove-Item Env:TMPCLEAN_IO_PERMITS -ErrorAction SilentlyContinue
    $run = Invoke-Tmpclean @()
    Write-Output "tmpclean report: $($run.Text)"
    $report = $run.Report
    Assert-That ($run.Code -eq 0) "exit code 0 (got $($run.Code))"
    Assert-That ($null -ne $report -and $report.schema -eq 1) 'JSON schema == 1'
    Assert-That ($null -ne $report -and $report.targets[0].status -eq 'resolved') 'user-temp resolved to the fixture'
    $kept = @($outsideFiles | Where-Object { Test-Path -LiteralPath $_ })
    Assert-That ($kept.Count -eq 3) "junction target's 3 files intact ($($kept.Count)/3)"
    Assert-That (-not (Test-Path -LiteralPath $junction)) 'junction link itself removed'
    Assert-That ($null -ne $report -and $report.counters.links_removed -ge 1) 'links_removed >= 1'
    Assert-That ($null -ne $report -and $report.counters.skipped_in_use -eq 1) "locked file counted in skipped_in_use ($($report.counters.skipped_in_use))"
    Assert-That (Test-Path -LiteralPath $locked) 'locked file still present'
    Assert-That (-not (Test-Path -LiteralPath $readonly)) 'read-only file gone'
    Assert-That (-not [IO.File]::Exists($longFile)) 'long-path file gone'
    Assert-That (-not (Test-Path -LiteralPath (Join-Path $root 'long'))) 'long-path directory chain gone'
    Assert-That (-not (Test-Path -LiteralPath (Join-Path $root 'n1'))) 'nested dirs gone'
    Assert-That ($null -ne $report -and $report.counters.failed -eq 0) "failed == 0 ($($report.counters.failed))"
    Assert-That (Test-Path -LiteralPath $root -PathType Container) 'temp root kept'
    if ($NegativeControl) {
        Assert-That (-not (Test-Path -LiteralPath $locked)) 'NEGATIVE CONTROL: locked file gone (must FAIL)'
    }

    Stop-Job $job
    Remove-Job $job -Force
    $job = $null

    if (-not $SkipBench) {
        $times = @{}
        foreach ($mode in 'single', 'auto') {
            Remove-Fixture
            [void][IO.Directory]::CreateDirectory($root)
            $seed = Measure-Command { New-BenchTree $BenchFiles }
            $extra = if ($mode -eq 'single') { @('--io-permits', '1') } else { @() }
            $bench = $null
            $elapsed = Measure-Command { $script:bench = Invoke-Tmpclean $extra }
            $bench = $script:bench
            $times[$mode] = $elapsed.TotalSeconds
            Write-Output ("BENCH {0}: {1:N3} s (seeded {2} files in {3:N1} s)" -f $mode, $elapsed.TotalSeconds, $BenchFiles, $seed.TotalSeconds)
            Assert-That ($bench.Code -eq 0) "bench $mode exit 0 (got $($bench.Code))"
            Assert-That ($null -ne $bench.Report -and $bench.Report.counters.files_removed -eq $BenchFiles) "bench $mode removed $BenchFiles files ($($bench.Report.counters.files_removed))"
            Assert-That (@(Get-ChildItem -LiteralPath $root -Force).Count -eq 0) "bench $mode left the root empty"
        }
        $ratio = $times['auto'] / $times['single']
        Write-Output ("BENCH single_s={0:N3} auto_s={1:N3} ratio={2:N3} threshold={3}" -f $times['single'], $times['auto'], $ratio, $MaxRatio)
        Assert-That ($ratio -le $MaxRatio) ("benchmark: auto <= {0}x single (ratio {1:N3})" -f $MaxRatio, $ratio)
    }
} finally {
    if ($null -ne $job) {
        Stop-Job $job -ErrorAction SilentlyContinue
        Remove-Job $job -Force -ErrorAction SilentlyContinue
    }
    $env:TMP = $savedTmp
    $env:TEMP = $savedTemp
    if ($null -ne $savedPermits) { $env:TMPCLEAN_IO_PERMITS = $savedPermits }
    Remove-Fixture
}

if ($script:failures -gt 0) {
    Write-Output "E2E FAILED: $($script:failures) assertion(s)"
    exit 1
}
Write-Output 'E2E PASSED'
exit 0
