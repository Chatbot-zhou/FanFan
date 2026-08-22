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

# 移除与 System32 冲突的旧版 VC 运行时 DLL。PyInstaller 会从依赖包收集到
# 旧版 msvcp140/vcruntime140（如 14.29 "cloudtest"），与系统新版（14.51）混用
# 会导致 onnxruntime 加载失败（"DLL load failed while importing
# onnxruntime_pybind11_state: 动态链接库(DLL)初始化例程失败"）。
# Windows 10/11 自带 VC 2015-2022 运行时，删除后由系统提供新版本。
foreach ($vcRuntime in @("msvcp140.dll", "MSVCP140_1.dll", "vcruntime140.dll", "vcruntime140_1.dll")) {
    $vcPath = Join-Path $internalDir $vcRuntime
    if (Test-Path -LiteralPath $vcPath) {
        Remove-Item -LiteralPath $vcPath -Force
    }
}

$workerSize = (Get-Item -LiteralPath $workerExecutable).Length
$internalSize = (Get-ChildItem -Path $internalDir -Recurse -File | Measure-Object -Property Length -Sum).Sum
Write-Output "Worker build checkpoint passed: $workerExecutable ($workerSize bytes, internal $([math]::Round($internalSize / 1MB)) MB)"
