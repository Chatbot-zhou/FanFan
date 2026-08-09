[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ExecutablePath,
    [uint32]$DurationSeconds = 600,
    [uint32]$WindowDeadlineSeconds = 5,
    [uint32]$SampleIntervalMs = 1000,
    [switch]$SkipSingleInstanceProbe
)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8
$executable = [IO.Path]::GetFullPath($ExecutablePath)
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw "Installed application not found: $executable"
}
if ($SampleIntervalMs -lt 100 -or $SampleIntervalMs -gt 5000) {
    throw 'SampleIntervalMs must be between 100 and 5000.'
}

function Get-ExactApplicationProcesses {
    Get-CimInstance Win32_Process -Filter "Name = 'remin-desktop.exe'" |
        Where-Object {
            $_.ExecutablePath -and
            [IO.Path]::GetFullPath($_.ExecutablePath).Equals($executable, [StringComparison]::OrdinalIgnoreCase)
        }
}

function Assert-HiddenBackgroundWindows {
    $unexpected = @()
    foreach ($name in @('remin-worker', 'llama-server')) {
        foreach ($process in @(Get-Process -Name $name -ErrorAction SilentlyContinue)) {
            if ($process.MainWindowHandle -ne 0) {
                $unexpected += "$($process.ProcessName):$($process.Id):$($process.MainWindowHandle)"
            }
        }
    }
    if ($unexpected.Count -gt 0) {
        throw "Background process exposed a window: $($unexpected -join ', ')"
    }
}

$primary = $null
$secondary = $null
$started = [Diagnostics.Stopwatch]::StartNew()
$unresponsiveSamples = 0
$maxWorkingSetBytes = 0L
$workerSamples = 0
$sampleCount = [Math]::Max(1, [Math]::Ceiling(($DurationSeconds * 1000.0) / $SampleIntervalMs))

try {
    $primary = Start-Process -FilePath $executable -WorkingDirectory (Split-Path -Parent $executable) -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds($WindowDeadlineSeconds)
    do {
        Start-Sleep -Milliseconds 100
        $primary.Refresh()
    } while (-not $primary.HasExited -and $primary.MainWindowHandle -eq 0 -and [DateTime]::UtcNow -lt $deadline)
    $visibleMs = $started.ElapsedMilliseconds
    if ($primary.HasExited) {
        throw "Installed application exited during startup: exit=$($primary.ExitCode)"
    }
    if ($primary.MainWindowHandle -eq 0) {
        throw "Installed application did not create its main window within $WindowDeadlineSeconds seconds."
    }
    if (-not $primary.Responding) {
        throw 'Installed application is not responding after its first visible frame.'
    }

    if (-not $SkipSingleInstanceProbe) {
        $secondary = Start-Process -FilePath $executable -WorkingDirectory (Split-Path -Parent $executable) -PassThru
        if (-not $secondary.WaitForExit(5000)) {
            throw "Second launch did not hand off to the existing instance: pid=$($secondary.Id)"
        }
        Start-Sleep -Milliseconds 500
        $instances = @(Get-ExactApplicationProcesses)
        if ($instances.Count -ne 1 -or $instances[0].ProcessId -ne $primary.Id) {
            throw "Single-instance checkpoint failed: expected_pid=$($primary.Id), actual=$($instances.ProcessId -join ',')"
        }
    }
    Assert-HiddenBackgroundWindows
    Write-Output "Startup checkpoint passed: visible_ms=$visibleMs responding=$($primary.Responding) single_instance_checked=$(-not $SkipSingleInstanceProbe) primary_pid=$($primary.Id)"

    for ($index = 1; $index -le $sampleCount; $index++) {
        Start-Sleep -Milliseconds $SampleIntervalMs
        $primary.Refresh()
        if ($primary.HasExited) {
            throw "Installed application exited during soak: sample=$index exit=$($primary.ExitCode)"
        }
        if (-not $primary.Responding) {
            $unresponsiveSamples++
        }
        $maxWorkingSetBytes = [Math]::Max($maxWorkingSetBytes, $primary.WorkingSet64)
        $workerSamples += @(Get-Process -Name 'remin-worker' -ErrorAction SilentlyContinue).Count
        Assert-HiddenBackgroundWindows
        if (($index % [Math]::Max(1, [Math]::Floor(30000 / $SampleIntervalMs))) -eq 0) {
            Write-Output "Runtime soak progress: samples=$index/$sampleCount unresponsive=$unresponsiveSamples max_working_set_mb=$([Math]::Round($maxWorkingSetBytes / 1MB, 1))"
        }
    }

    if ($unresponsiveSamples -ne 0) {
        throw "Installed application failed responsiveness gate: unresponsive_samples=$unresponsiveSamples/$sampleCount"
    }
    Write-Output "Runtime soak checkpoint passed: duration_seconds=$DurationSeconds samples=$sampleCount unresponsive=0 max_working_set_mb=$([Math]::Round($maxWorkingSetBytes / 1MB, 1)) worker_samples=$workerSamples background_windows=0"
}
finally {
    if ($secondary -and -not $secondary.HasExited) {
        Stop-Process -Id $secondary.Id -Force -ErrorAction SilentlyContinue
    }
    if ($primary -and -not $primary.HasExited) {
        Stop-Process -Id $primary.Id -Force -ErrorAction SilentlyContinue
        $primary.WaitForExit()
    }
}
