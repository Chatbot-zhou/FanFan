param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [string]$PythonExecutable = "python",
    [switch]$RequireSignature
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

function Get-PeSubsystem {
    param([Parameter(Mandatory = $true)][string]$ExecutablePath)

    $stream = [IO.File]::Open($ExecutablePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $reader = [IO.BinaryReader]::new($stream)
        try {
            $stream.Position = 0x3c
            $peOffset = $reader.ReadInt32()
            if ($peOffset -lt 0x40 -or $peOffset -gt ($stream.Length - 96)) {
                throw "Invalid PE header offset in $ExecutablePath"
            }
            $stream.Position = $peOffset
            if ($reader.ReadUInt32() -ne 0x00004550) {
                throw "Invalid PE signature in $ExecutablePath"
            }
            $optionalHeaderOffset = $peOffset + 24
            $stream.Position = $optionalHeaderOffset
            $magic = $reader.ReadUInt16()
            if ($magic -ne 0x10b -and $magic -ne 0x20b) {
                throw "Unsupported PE optional header in $ExecutablePath"
            }
            $stream.Position = $optionalHeaderOffset + 68
            return $reader.ReadUInt16()
        }
        finally {
            $reader.Dispose()
        }
    }
    finally {
        $stream.Dispose()
    }
}

function Assert-PeSubsystem {
    param(
        [Parameter(Mandatory = $true)][string]$ExecutablePath,
        [Parameter(Mandatory = $true)][UInt16]$Expected,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $actual = Get-PeSubsystem -ExecutablePath $ExecutablePath
    if ($actual -ne $Expected) {
        throw "$Label PE subsystem mismatch: expected=$Expected actual=$actual path=$ExecutablePath"
    }
    Write-Output "$Label PE subsystem checkpoint passed: subsystem=$actual"
}

function Assert-AssociatedIconMatches {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ExecutablePath,
        [Parameter(Mandatory = $true)]
        [string]$ExpectedIconPath,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    $associatedIcon = [Drawing.Icon]::ExtractAssociatedIcon($ExecutablePath)
    if ($null -eq $associatedIcon) {
        throw "$Label does not contain an associated Windows icon: $ExecutablePath"
    }
    try {
        $actualBitmap = $associatedIcon.ToBitmap()
        $expectedBitmap = [Drawing.Bitmap]::FromFile($ExpectedIconPath)
        try {
            if ($actualBitmap.Width -ne $expectedBitmap.Width -or $actualBitmap.Height -ne $expectedBitmap.Height) {
                throw "$Label icon dimensions differ from the canonical icon: actual=$($actualBitmap.Width)x$($actualBitmap.Height), expected=$($expectedBitmap.Width)x$($expectedBitmap.Height)"
            }
            $differentPixels = 0
            for ($y = 0; $y -lt $expectedBitmap.Height; $y++) {
                for ($x = 0; $x -lt $expectedBitmap.Width; $x++) {
                    if ($actualBitmap.GetPixel($x, $y).ToArgb() -ne $expectedBitmap.GetPixel($x, $y).ToArgb()) {
                        $differentPixels++
                    }
                }
            }
            if ($differentPixels -ne 0) {
                throw "$Label icon differs from the canonical icon at $differentPixels pixels."
            }
            Write-Output "$Label icon checkpoint passed: $($expectedBitmap.Width)x$($expectedBitmap.Height), different_pixels=0"
        }
        finally {
            $expectedBitmap.Dispose()
            $actualBitmap.Dispose()
        }
    }
    finally {
        $associatedIcon.Dispose()
    }
}

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$artifactRoot = [System.IO.Path]::GetFullPath((Join-Path $repositoryRoot ".artifacts"))
$installer = [System.IO.Path]::GetFullPath($InstallerPath)
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "Installer not found: $installer"
}
$canonicalIcon = Join-Path $repositoryRoot "apps\desktop\src-tauri\icons\32x32.png"
if (-not (Test-Path -LiteralPath $canonicalIcon -PathType Leaf)) {
    throw "Canonical application icon not found: $canonicalIcon"
}
Assert-AssociatedIconMatches -ExecutablePath $installer -ExpectedIconPath $canonicalIcon -Label "Installer"

$signature = Get-AuthenticodeSignature -LiteralPath $installer
if ($RequireSignature -and $signature.Status -ne "Valid") {
    throw "A valid Authenticode signature is required; status: $($signature.Status)"
}

$smokeId = [Guid]::NewGuid().ToString("N")
$smokeRoot = [System.IO.Path]::GetFullPath((Join-Path $artifactRoot "installer-smoke\$smokeId"))
if (-not $smokeRoot.StartsWith($artifactRoot + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Smoke target escaped the artifact root: $smokeRoot"
}

$installRoot = Join-Path $smokeRoot "app"
$localData = Join-Path $smokeRoot "localappdata"
$roamingData = Join-Path $smokeRoot "appdata"
New-Item -ItemType Directory -Force -Path $smokeRoot, $localData, $roamingData | Out-Null

$appProcess = $null
$installed = $false
$previousLocalAppData = $env:LOCALAPPDATA
$previousAppData = $env:APPDATA

try {
    $installProcess = Start-Process `
        -FilePath $installer `
        -ArgumentList @("/S", "/D=$installRoot") `
        -Wait `
        -PassThru `
        -WindowStyle Hidden
    if ($installProcess.ExitCode -ne 0) {
        throw "Silent installer failed with exit code $($installProcess.ExitCode)."
    }
    $installed = $true

    $desktopExecutable = Join-Path $installRoot "remin-desktop.exe"
    $workerExecutable = Join-Path $installRoot "worker\remin-worker.exe"
    $llamaRuntime = Join-Path $installRoot "runtime\llama"
    $llamaExecutable = Join-Path $llamaRuntime "llama-server.exe"
    $llamaManifest = Join-Path $llamaRuntime "MANIFEST.json"
    $llamaLicense = Join-Path $llamaRuntime "LICENSE-llama.cpp.txt"
    $uninstaller = Join-Path $installRoot "uninstall.exe"
    foreach ($requiredPath in @($desktopExecutable, $workerExecutable, $llamaExecutable, $llamaManifest, $llamaLicense, $uninstaller)) {
        if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
            throw "Installed payload missing: $requiredPath"
        }
    }
    Assert-AssociatedIconMatches -ExecutablePath $desktopExecutable -ExpectedIconPath $canonicalIcon -Label "Installed application"
    Assert-AssociatedIconMatches -ExecutablePath $uninstaller -ExpectedIconPath $canonicalIcon -Label "Uninstaller"
    Assert-PeSubsystem -ExecutablePath $desktopExecutable -Expected 2 -Label "Installed application"
    Assert-PeSubsystem -ExecutablePath $workerExecutable -Expected 3 -Label "Packaged worker"

    & (Join-Path $repositoryRoot "scripts\validate_llama_runtime.ps1") -RepositoryRoot $repositoryRoot -RuntimeRoot $llamaRuntime

    & $PythonExecutable (Join-Path $repositoryRoot "scripts\validate_packaged_worker.py") $workerExecutable
    if ($LASTEXITCODE -ne 0) {
        throw "The installed worker failed its protocol smoke test."
    }

    $env:LOCALAPPDATA = $localData
    $env:APPDATA = $roamingData
    $appProcess = Start-Process -FilePath $desktopExecutable -WorkingDirectory $installRoot -PassThru
    $windowDeadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        Start-Sleep -Milliseconds 100
        $appProcess.Refresh()
    } while (-not $appProcess.HasExited -and $appProcess.MainWindowHandle -eq 0 -and [DateTime]::UtcNow -lt $windowDeadline)
    if ($appProcess.HasExited) {
        throw "The installed desktop app exited early with code $($appProcess.ExitCode)."
    }
    if ($appProcess.MainWindowHandle -eq 0) {
        throw "The installed desktop app did not create its main window within 5 seconds."
    }
    if (-not $appProcess.Responding) {
        throw "The installed desktop app created a window but is not responding."
    }
    Write-Output "Installed application startup checkpoint passed: window_handle=$($appProcess.MainWindowHandle), responding=$($appProcess.Responding)"

    Stop-Process -Id $appProcess.Id -Force
    $appProcess.WaitForExit()
    $appProcess = $null

    $uninstallProcess = Start-Process `
        -FilePath $uninstaller `
        -ArgumentList "/S" `
        -Wait `
        -PassThru `
        -WindowStyle Hidden
    if ($uninstallProcess.ExitCode -ne 0) {
        throw "Silent uninstaller failed with exit code $($uninstallProcess.ExitCode)."
    }
    $installed = $false
    Start-Sleep -Seconds 2
    if (Test-Path -LiteralPath $installRoot) {
        throw "The uninstaller left the application directory behind: $installRoot"
    }

    $hash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Output "Installer smoke checkpoint passed."
    Write-Output "installer=$installer"
    Write-Output "sha256=$hash"
    Write-Output "signature_status=$($signature.Status)"
    Write-Output "smoke_root=$smokeRoot"
}
finally {
    if ($appProcess -and -not $appProcess.HasExited) {
        Stop-Process -Id $appProcess.Id -Force -ErrorAction SilentlyContinue
    }
    if ($installed) {
        $fallbackUninstaller = Join-Path $installRoot "uninstall.exe"
        if (Test-Path -LiteralPath $fallbackUninstaller -PathType Leaf) {
            Start-Process -FilePath $fallbackUninstaller -ArgumentList "/S" -Wait -WindowStyle Hidden
        }
    }
    $env:LOCALAPPDATA = $previousLocalAppData
    $env:APPDATA = $previousAppData
}
