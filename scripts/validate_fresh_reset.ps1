param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [int]$RespondingSeconds = 30,
    [string]$InstallRoot = ""
)

$ErrorActionPreference = "Stop"

function Assert-DirectChild {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Parent,
        [Parameter(Mandatory = $true)][string]$Leaf
    )
    $resolved = [IO.Path]::GetFullPath($Path)
    if ((Split-Path -Parent $resolved) -ne [IO.Path]::GetFullPath($Parent) -or
        (Split-Path -Leaf $resolved) -ne $Leaf) {
        throw "Unsafe app-data path: $resolved"
    }
    return $resolved
}

function Stop-ReminProcesses {
    $owned = Get-CimInstance Win32_Process | Where-Object {
        $_.Name -in @("remin-desktop.exe", "remin-worker.exe", "llama-server.exe") -or
        ($_.Name -like "python*.exe" -and $_.CommandLine -like "*remin_worker*")
    }
    foreach ($process in $owned) {
        Stop-Process -Id ([int]$process.ProcessId) -Force -ErrorAction SilentlyContinue
    }
}

function Get-SourceHashSample {
    param([Parameter(Mandatory = $true)][string]$Workspace)
    $roots = @(
        [Environment]::GetFolderPath("MyDocuments")
        [Environment]::GetFolderPath("MyPictures")
        (Join-Path ([Environment]::GetFolderPath("UserProfile")) "Downloads")
        [Environment]::GetFolderPath("Desktop")
    ) | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Container) } |
        Select-Object -Unique
    $samples = @()
    foreach ($root in $roots) {
        $candidates = Get-ChildItem -LiteralPath $root -File -Recurse -ErrorAction SilentlyContinue |
            Where-Object {
                $_.Length -gt 0 -and $_.Length -le 50MB -and
                -not $_.FullName.StartsWith($Workspace, [StringComparison]::OrdinalIgnoreCase)
            } | Select-Object -First 3
        foreach ($file in $candidates) {
            try {
                $samples += [pscustomobject]@{
                    Path = $file.FullName
                    Length = $file.Length
                    Modified = $file.LastWriteTimeUtc
                    Hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
                }
            }
            catch {
                # A locked candidate is skipped; no source file is ever opened for writing.
            }
        }
    }
    if ($samples.Count -lt 3) {
        throw "Only $($samples.Count) source files were available for the hash sample."
    }
    return $samples
}

function Find-InstalledExecutable {
    param([string]$ExpectedInstallRoot)
    if ($ExpectedInstallRoot) {
        $expected = Join-Path $ExpectedInstallRoot "remin-desktop.exe"
        if (Test-Path -LiteralPath $expected -PathType Leaf) { return $expected }
    }
    $entry = Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*" -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -eq "拾忆" } | Select-Object -First 1
    $candidates = @()
    if ($entry.InstallLocation) {
        $candidates += Join-Path $entry.InstallLocation "remin-desktop.exe"
    }
    if ($entry.UninstallString) {
        $raw = $entry.UninstallString.Trim()
        $uninstaller = if ($raw -match '^"([^"]+)"') { $Matches[1] } else { $raw.Split(" ")[0] }
        if ($uninstaller) {
            $candidates += Join-Path (Split-Path -Parent $uninstaller) "remin-desktop.exe"
        }
    }
    $candidates += Join-Path $env:LOCALAPPDATA "拾忆\remin-desktop.exe"
    $candidates += Join-Path $env:LOCALAPPDATA "Programs\拾忆\remin-desktop.exe"
    return $candidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
}

$installer = [IO.Path]::GetFullPath($InstallerPath)
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "Installer not found: $installer"
}
if ($RespondingSeconds -lt 5 -or $RespondingSeconds -gt 300) {
    throw "RespondingSeconds must be between 5 and 300."
}

$workspace = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$programsBase = [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA "Programs"))
if (-not $InstallRoot) { $InstallRoot = Join-Path $programsBase "拾忆" }
$installLeaf = Split-Path -Leaf ([IO.Path]::GetFullPath($InstallRoot))
$installRootResolved = Assert-DirectChild -Path $InstallRoot -Parent $programsBase -Leaf $installLeaf
if (Test-Path -LiteralPath $installRootResolved) {
    throw "Fresh install target already exists: $installRootResolved"
}
$samples = Get-SourceHashSample -Workspace $workspace
$roamingBase = [IO.Path]::GetFullPath($env:APPDATA)
$localBase = [IO.Path]::GetFullPath($env:LOCALAPPDATA)
$roaming = Assert-DirectChild -Path (Join-Path $roamingBase "com.remin.desktop") -Parent $roamingBase -Leaf "com.remin.desktop"
$local = Assert-DirectChild -Path (Join-Path $localBase "com.remin.desktop") -Parent $localBase -Leaf "com.remin.desktop"
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$roamingLeaf = "com.remin.desktop.isolated-$stamp"
$localLeaf = "com.remin.desktop.isolated-$stamp"
$roamingQuarantine = Assert-DirectChild -Path (Join-Path $roamingBase $roamingLeaf) -Parent $roamingBase -Leaf $roamingLeaf
$localQuarantine = Assert-DirectChild -Path (Join-Path $localBase $localLeaf) -Parent $localBase -Leaf $localLeaf
foreach ($target in @($roamingQuarantine, $localQuarantine)) {
    if (Test-Path -LiteralPath $target) { throw "Quarantine already exists: $target" }
}

Stop-ReminProcesses
Start-Sleep -Seconds 1
$moved = @()
$primary = $null
try {
    foreach ($pair in @(@($roaming, $roamingQuarantine), @($local, $localQuarantine))) {
        if (Test-Path -LiteralPath $pair[0] -PathType Container) {
            $bytes = (Get-ChildItem -LiteralPath $pair[0] -Recurse -File -ErrorAction SilentlyContinue |
                Measure-Object Length -Sum).Sum
            Move-Item -LiteralPath $pair[0] -Destination $pair[1]
            if ((Test-Path -LiteralPath $pair[0]) -or -not (Test-Path -LiteralPath $pair[1])) {
                throw "App-data quarantine move did not complete: $($pair[0])"
            }
            $moved += [pscustomobject]@{
                Source = $pair[0]
                Quarantine = $pair[1]
                Bytes = [uint64]$bytes
            }
        }
    }

    $install = Start-Process -FilePath $installer -ArgumentList @("/S", "/D=$installRootResolved") -Wait -PassThru -WindowStyle Hidden
    if ($install.ExitCode -ne 0) { throw "Fresh installer failed: $($install.ExitCode)" }
    $desktopExecutable = Find-InstalledExecutable -ExpectedInstallRoot $installRootResolved
    if (-not $desktopExecutable) { throw "Fresh installed executable was not found." }

    $watch = [Diagnostics.Stopwatch]::StartNew()
    $primary = Start-Process -FilePath $desktopExecutable -WorkingDirectory (Split-Path -Parent $desktopExecutable) -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        Start-Sleep -Milliseconds 100
        $primary.Refresh()
    } while (-not $primary.HasExited -and $primary.MainWindowHandle -eq 0 -and [DateTime]::UtcNow -lt $deadline)
    if ($primary.HasExited -or $primary.MainWindowHandle -eq 0) {
        throw "Fresh installed app did not show its main window within five seconds."
    }
    $visibleMs = $watch.ElapsedMilliseconds
    $responding = 0
    for ($index = 0; $index -lt $RespondingSeconds; $index++) {
        Start-Sleep -Seconds 1
        $primary.Refresh()
        if ($primary.HasExited) { throw "Fresh installed app exited during validation." }
        if ($primary.Responding) { $responding++ }
    }
    if ($responding -ne $RespondingSeconds) {
        throw "Fresh app response samples failed: $responding/$RespondingSeconds"
    }

    $second = Start-Process -FilePath $desktopExecutable -WorkingDirectory (Split-Path -Parent $desktopExecutable) -PassThru
    if (-not $second.WaitForExit(5000) -or $second.ExitCode -ne 0) {
        throw "Fresh installed second instance did not exit cleanly."
    }
    Stop-Process -Id $primary.Id -Force
    $primary = $null
    Start-Sleep -Seconds 1
    Stop-ReminProcesses

    foreach ($sample in $samples) {
        $current = Get-Item -LiteralPath $sample.Path
        $currentHash = (Get-FileHash -LiteralPath $sample.Path -Algorithm SHA256).Hash
        if ($current.Length -ne $sample.Length -or
            $current.LastWriteTimeUtc -ne $sample.Modified -or
            $currentHash -ne $sample.Hash) {
            throw "A sampled source file changed during reset/install validation."
        }
    }

    $deletedBytes = [uint64]0
    foreach ($item in $moved) {
        $expectedParent = if ($item.Source -eq $roaming) { $roamingBase } else { $localBase }
        $expectedLeaf = if ($item.Source -eq $roaming) { $roamingLeaf } else { $localLeaf }
        $resolved = Assert-DirectChild -Path $item.Quarantine -Parent $expectedParent -Leaf $expectedLeaf
        $deletedBytes += $item.Bytes
        Remove-Item -LiteralPath $resolved -Recurse -Force
        if (Test-Path -LiteralPath $resolved) { throw "Quarantine deletion failed: $resolved" }
    }

    Write-Output "Fresh reset checkpoint passed."
    Write-Output "sample_hashes=$($samples.Count)"
    Write-Output "quarantines_deleted=$($moved.Count)"
    Write-Output "deleted_bytes=$deletedBytes"
    Write-Output "fresh_visible_ms=$visibleMs"
    Write-Output "responding=$responding/$RespondingSeconds"
    Write-Output "installed_executable=$desktopExecutable"
}
catch {
    if ($moved.Count -gt 0) {
        Write-Warning "Validation stopped before deletion. Old app data remains in the timestamped quarantine directories."
        foreach ($item in $moved) { Write-Warning $item.Quarantine }
    }
    throw
}
finally {
    if ($primary -and -not $primary.HasExited) {
        Stop-Process -Id $primary.Id -Force -ErrorAction SilentlyContinue
    }
}
