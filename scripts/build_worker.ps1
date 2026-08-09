param(
    [string]$PythonExecutable = "python"
)

$ErrorActionPreference = "Stop"
$env:PYTHONNOUSERSITE = "1"
$env:PYTHONUTF8 = "1"
$env:PYTHONIOENCODING = "utf-8"

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$artifactRoot = Join-Path $repositoryRoot ".artifacts"
$isolatedAppData = Join-Path $artifactRoot "python-appdata"
$isolatedLocalAppData = Join-Path $artifactRoot "python-localappdata"
$env:APPDATA = $isolatedAppData
$env:LOCALAPPDATA = $isolatedLocalAppData
$venvRoot = Join-Path $artifactRoot "packaging-venv"
$venvPython = Join-Path $venvRoot "Scripts\python.exe"
$workerRoot = Join-Path $repositoryRoot "services\worker"
$entryPoint = Join-Path $workerRoot "remin_worker_entry.py"
$requirements = Join-Path $workerRoot "requirements-build.txt"
$ocrScript = Join-Path $workerRoot "remin_worker\windows_ocr.ps1"
$distRoot = Join-Path $artifactRoot "worker"
$workRoot = Join-Path $artifactRoot "pyinstaller\work"
$specRoot = Join-Path $artifactRoot "pyinstaller\spec"
$workerExecutable = Join-Path $distRoot "remin-worker\remin-worker.exe"
$iconPath = Join-Path $repositoryRoot "apps\desktop\src-tauri\icons\icon.ico"

New-Item -ItemType Directory -Force -Path $isolatedAppData, $isolatedLocalAppData | Out-Null

if (-not (Test-Path -LiteralPath $venvPython)) {
    & $PythonExecutable -m venv $venvRoot
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to create the worker build virtual environment."
    }
}

$pythonVersion = & $venvPython -c "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')"
if ($LASTEXITCODE -ne 0 -or $pythonVersion.Trim() -ne "3.12") {
    throw "The worker build requires Python 3.12; current version: $pythonVersion"
}

& $venvPython -I -m pip install --disable-pip-version-check --no-deps --requirement $requirements
if ($LASTEXITCODE -ne 0) {
    throw "Failed to install worker build dependencies."
}

New-Item -ItemType Directory -Force -Path $distRoot, $workRoot, $specRoot | Out-Null

& $venvPython -I -m PyInstaller `
    --noconfirm `
    --clean `
    --onedir `
    --console `
    --noupx `
    --name "remin-worker" `
    --icon $iconPath `
    --add-data "$ocrScript;remin_worker" `
    --paths $workerRoot `
    --distpath $distRoot `
    --workpath $workRoot `
    --specpath $specRoot `
    $entryPoint
if ($LASTEXITCODE -ne 0) {
    throw "Failed to build the standalone worker."
}

if (-not (Test-Path -LiteralPath $workerExecutable)) {
    throw "The worker build completed without the expected output: $workerExecutable"
}

$workerSize = (Get-Item -LiteralPath $workerExecutable).Length
Write-Output "Worker build checkpoint passed: $workerExecutable ($workerSize bytes)"
