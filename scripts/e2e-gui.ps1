# Real-surface QA for tmpclean-gui.exe via UI Automation (windows-latest CI).
# Every wait is bounded and polls at a 250 ms tick; no long fixed sleeps.
# Windows PowerShell 5.1 compatible. Exits non-zero on any failed assertion.
param([string]$Exe = 'target\release\tmpclean-gui.exe')

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Drawing, System.Windows.Forms

$Exe = (Resolve-Path -LiteralPath $Exe).Path
$fixture = Join-Path $env:RUNNER_TEMP 'gui\Temp'
$shot = Join-Path $env:RUNNER_TEMP 'task-10-gui.png'
$script:procs = @()
$AE = [System.Windows.Automation.AutomationElement]

# Managed UIA needs the Win32 client-side proxies to expose Button/Text + InvokePattern.
try {
    $asm = [System.Reflection.Assembly]::Load('UIAutomationClientsideProviders, Version=4.0.0.0, Culture=neutral, PublicKeyToken=31bf3856ad364e35')
    [System.Windows.Automation.ClientSettings]::RegisterClientSideProviderAssembly($asm.GetName())
    Write-Host 'UIA client-side proxies registered'
} catch { Write-Host "UIA client-side proxy registration failed: $_" }

Add-Type -Namespace E2E -Name Native -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern System.IntPtr SendMessageW(System.IntPtr hwnd, uint msg, System.IntPtr w, System.IntPtr l);
'@

function Check($cond, [string]$msg) {
    if (-not $cond) { throw "ASSERT FAIL: $msg" }
    Write-Host "ASSERT OK: $msg"
}

function Count-Fixture { @(Get-ChildItem -LiteralPath $fixture -Force).Count }

function Seed {
    if (Test-Path -LiteralPath $fixture) { Remove-Item -LiteralPath $fixture -Recurse -Force }
    New-Item -ItemType Directory -Path $fixture | Out-Null
    foreach ($i in 1..200) { Set-Content -LiteralPath (Join-Path $fixture "f$i.tmp") -Value "file $i" }
    Check ((Count-Fixture) -eq 200) "seeded 200 files in $fixture"
}

function Launch([string]$arguments) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo $Exe
    $psi.Arguments = $arguments
    $psi.UseShellExecute = $false
    $psi.EnvironmentVariables['TMP'] = $fixture
    $psi.EnvironmentVariables['TEMP'] = $fixture
    $p = [System.Diagnostics.Process]::Start($psi)
    $script:procs += $p
    Write-Host "launched pid $($p.Id): tmpclean-gui.exe $arguments"
    $p
}

# Bounded 30 s: WaitForInputIdle, then poll the process main window handle.
function Wait-Window($p) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try { [void]$p.WaitForInputIdle(30000) } catch { }
    while ($sw.Elapsed.TotalSeconds -lt 30) {
        $p.Refresh()
        if ($p.HasExited) { throw "process exited early with code $($p.ExitCode)" }
        if ($p.MainWindowHandle -ne [IntPtr]::Zero) {
            return $AE::FromHandle($p.MainWindowHandle)
        }
        Start-Sleep -Milliseconds 250
    }
    throw 'timeout (30 s) waiting for the main window'
}

function Find-Child($win, [string]$automationId) {
    $cond = New-Object System.Windows.Automation.PropertyCondition($AE::AutomationIdProperty, $automationId)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt 10) {
        $el = $win.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $cond)
        if ($el) { return $el }
        Start-Sleep -Milliseconds 250
    }
    throw "control with AutomationId $automationId not found"
}

function Dump-Tree($win) {
    foreach ($el in $win.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)) {
        Write-Host ("  uia: id={0} type={1} enabled={2} name={3}" -f $el.Current.AutomationId, $el.Current.ControlType.ProgrammaticName, $el.Current.IsEnabled, ($el.Current.Name -replace "`r?`n", ' | '))
    }
}

# Label text by prefix, from any descendant's UIA Name (STATIC = window text).
function Find-Text($win, [string]$prefix) {
    foreach ($el in $win.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)) {
        $name = [string]$el.Current.Name
        if ($name.StartsWith($prefix)) { return $name }
    }
    $null
}

function Invoke-El($el) {
    $name = $el.Current.Name
    $type = $el.Current.ControlType.ProgrammaticName
    $pattern = $null
    if ($el.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) {
        $pattern.Invoke()
        Write-Host "invoked '$name' via UIA InvokePattern"
    } else {
        # BM_CLICK = 0x00F5: the same click a user/InvokePattern produces.
        [void][E2E.Native]::SendMessageW([IntPtr]$el.Current.NativeWindowHandle, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)
        Write-Host "invoked '$name' via BM_CLICK (InvokePattern unsupported, type $type)"
    }
}

function Wait-Exit($p, [int]$expected, [string]$what) {
    Check ($p.WaitForExit(30000)) "$what exited within 30 s"
    Check ($p.ExitCode -eq $expected) "$what exit code $($p.ExitCode) == $expected"
}

$code = 0
try {
    # --- happy path: --yes run empties the fixture ---------------------------
    Seed
    $p = Launch '--targets user-temp --min-age 0 --yes'
    $win = Wait-Window $p
    $close = Find-Child $win '102'
    Check ($close.Current.Name -eq 'Close') "Close button (IDC_CLOSE=102) accessible name is 'Close'"
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while (-not ($win.Current.Name -eq 'tmpclean - done' -and $close.Current.IsEnabled)) {
        if ($p.HasExited) { throw "GUI exited before completion with code $($p.ExitCode)" }
        if ($sw.Elapsed.TotalSeconds -ge 120) {
            throw "timeout (120 s): title '$($win.Current.Name)', Close IsEnabled=$($close.Current.IsEnabled)"
        }
        Start-Sleep -Milliseconds 250
    }
    Check ($win.Current.Name -eq 'tmpclean - done') "title is 'tmpclean - done' after $([int]$sw.Elapsed.TotalMilliseconds) ms"
    Check ($close.Current.IsEnabled) 'Close IsEnabled == true at completion'
    $r = $win.Current.BoundingRectangle
    $bmp = New-Object System.Drawing.Bitmap ([int]$r.Width), ([int]$r.Height)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen([int]$r.X, [int]$r.Y, 0, 0, $bmp.Size)
    $bmp.Save($shot, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    Check ((Get-Item -LiteralPath $shot).Length -gt 0) "screenshot saved to $shot"
    Dump-Tree $win
    $status = Find-Text $win 'Done:'
    Write-Host "status label: $status"
    Check ($status -like 'Done: completed*(exit code 0)') 'status label reports completed, exit code 0'
    $counters = Find-Text $win 'Files removed:'
    Write-Host "counters label: $counters"
    Check ($counters -like 'Files removed: 200*') 'counters label shows Files removed: 200'
    Invoke-El $close
    Wait-Exit $p 0 'happy run (Close clicked)'
    Check (Test-Path -LiteralPath $fixture) 'fixture root kept'
    Check ((Count-Fixture) -eq 0) 'fixture is empty'

    # --- negative: Esc during the countdown -> exit 3, nothing touched -------
    Seed
    $p = Launch '--targets user-temp --min-age 0'
    $win = Wait-Window $p
    $close = Find-Child $win '102'
    $countdown = Find-Text $win 'Starting in '
    Write-Host "countdown label: $countdown"
    if ($null -eq $countdown) { Dump-Tree $win }
    Check ($null -ne $countdown) 'countdown label visible without --yes'
    Check (-not $close.Current.IsEnabled) 'Close disabled during the countdown'
    Check ($win.Current.Name -eq 'tmpclean') "title is 'tmpclean' before completion"
    try { $win.SetFocus() } catch { Write-Host "SetFocus: $_" }
    [System.Windows.Forms.SendKeys]::SendWait('{ESC}')
    Wait-Exit $p 3 'Esc during countdown'
    Check ((Count-Fixture) -eq 200) 'fixture intact after Esc (200 files)'

    # --- negative: Cancel button during the countdown -> exit 3 --------------
    $p = Launch '--targets user-temp --min-age 0'
    $win = Wait-Window $p
    Invoke-El (Find-Child $win '101')
    Wait-Exit $p 3 'Cancel during countdown'
    Check ((Count-Fixture) -eq 200) 'fixture intact after Cancel (200 files)'

    # --- negative: malformed flag -> usage message box, exit 2 ---------------
    $p = Launch '--targets user-temp --min-age soon --yes'
    $box = Wait-Window $p
    Write-Host "usage dialog title: $($box.Current.Name)"
    Invoke-El (Find-Child $box '2')
    Wait-Exit $p 2 'malformed --min-age'
    Check ((Count-Fixture) -eq 200) 'fixture intact after usage error (200 files)'

    Write-Host 'E2E-GUI PASS'
} catch {
    Write-Host "E2E-GUI FAIL: $_"
    $code = 1
} finally {
    foreach ($p in $script:procs) {
        if (-not $p.HasExited) { Write-Host "killing leftover pid $($p.Id)"; $p.Kill() }
    }
}
exit $code
