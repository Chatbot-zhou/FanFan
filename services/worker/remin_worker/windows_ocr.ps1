param(
    [Parameter(Mandatory = $true)][string]$SourcePath,
    [Parameter(Mandatory = $true)][ValidateSet('image', 'pdf')][string]$SourceKind,
    [int]$MaxPages = 50,
    [string]$LanguageTag = 'zh-Hans-CN',
    [string]$PageNumbers = '',
    [string]$AssetCacheDir = ''
)

$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8
Add-Type -AssemblyName System.Runtime.WindowsRuntime

function Await-Operation($Operation, [Type]$ResultType) {
    $method = [System.WindowsRuntimeSystemExtensions].GetMethods() |
        Where-Object {
            $_.Name -eq 'AsTask' -and $_.IsGenericMethod -and
            $_.GetParameters().Count -eq 1 -and
            $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
        } |
        Select-Object -First 1
    if ($null -eq $method) { throw 'WinRT async operation adapter is unavailable' }
    $task = $method.MakeGenericMethod($ResultType).Invoke($null, @($Operation))
    $task.GetAwaiter().GetResult()
}

function Await-Action($Action) {
    $method = [System.WindowsRuntimeSystemExtensions].GetMethods() |
        Where-Object {
            $_.Name -eq 'AsTask' -and -not $_.IsGenericMethod -and
            $_.GetParameters().Count -eq 1 -and
            $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncAction'
        } |
        Select-Object -First 1
    if ($null -eq $method) { throw 'WinRT async action adapter is unavailable' }
    $task = $method.Invoke($null, @($Action))
    $null = $task.GetAwaiter().GetResult()
}

[void][Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]
[void][Windows.Storage.FileAccessMode, Windows.Storage, ContentType = WindowsRuntime]
[void][Windows.Storage.Streams.IRandomAccessStream, Windows.Storage.Streams, ContentType = WindowsRuntime]
[void][Windows.Storage.Streams.InMemoryRandomAccessStream, Windows.Storage.Streams, ContentType = WindowsRuntime]
[void][Windows.Storage.Streams.DataReader, Windows.Storage.Streams, ContentType = WindowsRuntime]
[void][Windows.Graphics.Imaging.BitmapDecoder, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
[void][Windows.Graphics.Imaging.SoftwareBitmap, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
[void][Windows.Media.Ocr.OcrEngine, Windows.Foundation, ContentType = WindowsRuntime]
[void][Windows.Media.Ocr.OcrResult, Windows.Foundation, ContentType = WindowsRuntime]
[void][Windows.Globalization.Language, Windows.Globalization, ContentType = WindowsRuntime]
[void][Windows.Data.Pdf.PdfDocument, Windows.Data.Pdf, ContentType = WindowsRuntime]
[void][Windows.Data.Pdf.PdfPageRenderOptions, Windows.Data.Pdf, ContentType = WindowsRuntime]

$available = [Windows.Media.Ocr.OcrEngine]::AvailableRecognizerLanguages
$language = $available | Where-Object { $_.LanguageTag -eq $LanguageTag } | Select-Object -First 1
if ($null -eq $language) {
    $language = $available | Where-Object { $_.LanguageTag -like 'zh-Hans*' } | Select-Object -First 1
}
if ($null -eq $language) {
    $language = $available | Select-Object -First 1
}
if ($null -eq $language) {
    # Keep this file pure ASCII: Windows PowerShell 5.1 reads BOM-less scripts
    # with the ANSI code page, and UTF-8 Chinese bytes would break parsing on
    # zh-CN systems. The user-facing message lives in ocr.py (OCR_LANGUAGE_PACK_MISSING).
    Write-Error -ErrorAction Continue 'OCR_LANGUAGE_PACK_MISSING: Windows OCR language pack is not installed. Add Simplified Chinese OCR in Settings > Time & Language > Language & region and retry.'
    exit 3
}
$engine = [Windows.Media.Ocr.OcrEngine]::TryCreateFromLanguage($language)
if ($null -eq $engine) { throw 'Windows OCR engine initialization failed' }

function Read-Bitmap($Bitmap, [int]$PageNumber) {
    $result = Await-Operation ($engine.RecognizeAsync($Bitmap)) ([Windows.Media.Ocr.OcrResult])
    $lines = @()
    foreach ($line in $result.Lines) {
        $words = @($line.Words)
        if ($words.Count -eq 0) { continue }
        $x0 = ($words | ForEach-Object { $_.BoundingRect.X } | Measure-Object -Minimum).Minimum
        $y0 = ($words | ForEach-Object { $_.BoundingRect.Y } | Measure-Object -Minimum).Minimum
        $x1 = ($words | ForEach-Object { $_.BoundingRect.X + $_.BoundingRect.Width } | Measure-Object -Maximum).Maximum
        $y1 = ($words | ForEach-Object { $_.BoundingRect.Y + $_.BoundingRect.Height } | Measure-Object -Maximum).Maximum
        $lines += [ordered]@{
            page_no = $PageNumber
            text = $line.Text
            bbox = [ordered]@{
                x0 = [Math]::Max(0.0, [Math]::Min(1.0, $x0 / $Bitmap.PixelWidth))
                y0 = [Math]::Max(0.0, [Math]::Min(1.0, $y0 / $Bitmap.PixelHeight))
                x1 = [Math]::Max(0.0, [Math]::Min(1.0, $x1 / $Bitmap.PixelWidth))
                y1 = [Math]::Max(0.0, [Math]::Min(1.0, $y1 / $Bitmap.PixelHeight))
            }
        }
    }
    return $lines
}

$file = Await-Operation ([Windows.Storage.StorageFile]::GetFileFromPathAsync($SourcePath)) ([Windows.Storage.StorageFile])
$allLines = @()
$renderedPages = @()
$pageCount = 0
if ($SourceKind -eq 'image') {
    $stream = Await-Operation ($file.OpenAsync([Windows.Storage.FileAccessMode]::Read)) ([Windows.Storage.Streams.IRandomAccessStream])
    try {
        $decoder = Await-Operation ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder])
        $bitmap = Await-Operation ($decoder.GetSoftwareBitmapAsync()) ([Windows.Graphics.Imaging.SoftwareBitmap])
        try { $allLines = @(Read-Bitmap $bitmap 1); $pageCount = 1 } finally { $bitmap.Dispose() }
    } finally { $stream.Dispose() }
} else {
    $document = Await-Operation ([Windows.Data.Pdf.PdfDocument]::LoadFromFileAsync($file)) ([Windows.Data.Pdf.PdfDocument])
    $availablePageCount = [Math]::Min([int]$document.PageCount, $MaxPages)
    $selectedIndexes = if ([string]::IsNullOrWhiteSpace($PageNumbers)) {
        @(0..($availablePageCount - 1))
    } else {
        @($PageNumbers.Split(',') | ForEach-Object { [int]$_ - 1 } | Where-Object { $_ -ge 0 -and $_ -lt $availablePageCount } | Select-Object -Unique)
    }
    $pageCount = $selectedIndexes.Count
    foreach ($index in $selectedIndexes) {
        $page = $document.GetPage($index)
        $memory = [Windows.Storage.Streams.InMemoryRandomAccessStream]::new()
        try {
            $options = [Windows.Data.Pdf.PdfPageRenderOptions]::new()
            $width = [Math]::Min(2200, [Math]::Max(1200, [int]($page.Size.Width * 2)))
            $options.DestinationWidth = $width
            Await-Action ($page.RenderToStreamAsync($memory, $options))
            if (-not [string]::IsNullOrWhiteSpace($AssetCacheDir)) {
                $null = [IO.Directory]::CreateDirectory($AssetCacheDir)
                if ($memory.Size -le 0 -or $memory.Size -gt 67108864) { throw 'Rendered PDF page exceeds the safe cache limit' }
                $renderedPath = Join-Path $AssetCacheDir ("ocr-page-{0}-{1}.png" -f ($index + 1), [Guid]::NewGuid().ToString('N'))
                $input = $memory.GetInputStreamAt(0)
                $reader = [Windows.Storage.Streams.DataReader]::new($input)
                try {
                    $loaded = Await-Operation ($reader.LoadAsync([uint32]$memory.Size)) ([uint32])
                    if ($loaded -ne [uint32]$memory.Size) { throw 'Rendered PDF page was not read completely' }
                    $bytes = New-Object byte[] ([int]$loaded)
                    $reader.ReadBytes($bytes)
                    [IO.File]::WriteAllBytes($renderedPath, $bytes)
                    $renderedPages += [ordered]@{ page_no = ($index + 1); path = $renderedPath; mime_type = 'image/png' }
                } finally {
                    $reader.Dispose()
                    $input.Dispose()
                }
            }
            $memory.Seek(0)
            $decoder = Await-Operation ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($memory)) ([Windows.Graphics.Imaging.BitmapDecoder])
            $bitmap = Await-Operation ($decoder.GetSoftwareBitmapAsync()) ([Windows.Graphics.Imaging.SoftwareBitmap])
            try { $allLines += @(Read-Bitmap $bitmap ($index + 1)) } finally { $bitmap.Dispose() }
        } finally {
            $memory.Dispose()
            $page.Dispose()
        }
    }
}

[ordered]@{
    language = $language.LanguageTag
    page_count = $pageCount
    lines = $allLines
    rendered_pages = $renderedPages
} | ConvertTo-Json -Depth 8 -Compress
