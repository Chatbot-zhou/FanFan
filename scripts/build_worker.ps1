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
$entryPoint = Join-Path $workerRoot "fanfan_worker_entry.py"
$requirements = Join-Path $workerRoot "requirements-build.txt"
$ocrScript = Join-Path $workerRoot "fanfan_worker\windows_ocr.ps1"
$distRoot = Join-Path $artifactRoot "worker"
$workRoot = Join-Path $artifactRoot "pyinstaller\work"
$specRoot = Join-Path $artifactRoot "pyinstaller\spec"
$workerExecutable = Join-Path $distRoot "fanfan-worker\fanfan-worker.exe"
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

& $venvPython -I -m pip install --disable-pip-version-check --upgrade "pip==26.1.2"
if ($LASTEXITCODE -ne 0) {
    throw "Failed to install the pinned worker package installer."
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
    --name "fanfan-worker" `
    --icon $iconPath `
    --add-data "$ocrScript;fanfan_worker" `
    --collect-all "rapidocr" `
    --collect-all "sherpa_onnx" `
    --collect-all "nvidia" `
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

# Flatten NVIDIA CUDA/cuDNN runtime DLLs into _internal so that LoadLibrary by
# relative name resolves them (the bootloader prepends _internal to PATH).
$internalDir = Join-Path $distRoot "fanfan-worker\_internal"
$nvidiaRoot = Join-Path $venvRoot "Lib\site-packages\nvidia"
if (Test-Path -LiteralPath $nvidiaRoot) {
    Get-ChildItem -Path $nvidiaRoot -Recurse -Filter *.dll |
        Copy-Item -Destination $internalDir -Force
}

# cuDNN 9 on Windows requires zlibwapi.dll, which the nvidia pip packages do not
# ship. Search common locations; the build fails loudly if it cannot be found.
$zlibSource = $null
$zlibCandidates = @(
    (Join-Path $internalDir "zlibwapi.dll"),
    (Join-Path $venvRoot "Scripts\zlibwapi.dll"),
    "C:\Program Files\QuarkCloudDrive\6.5.1.711\zlibwapi.dll"
)
foreach ($candidate in $zlibCandidates) {
    if (Test-Path -LiteralPath $candidate) {
        $zlibSource = $candidate
        break
    }
}
if (-not $zlibSource) {
    throw "zlibwapi.dll (required by cuDNN 9) was not found; install it next to the venv or edit scripts/build_worker.ps1"
}
Copy-Item -LiteralPath $zlibSource -Destination $internalDir -Force

$workerSize = (Get-Item -LiteralPath $workerExecutable).Length
$internalSize = (Get-ChildItem -Path $internalDir -Recurse -File | Measure-Object -Property Length -Sum).Sum
Write-Output "Worker build checkpoint passed: $workerExecutable ($workerSize bytes, internal $([math]::Round($internalSize / 1MB)) MB)"
