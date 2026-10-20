# Build + collect every runtime dependency into .\dist so the folder can be
# copied to another Windows PC as-is.
#
#   .\package.ps1                    # exe + onnxruntime
#   .\package.ps1 -IncludeModels     # + *.onnx in repo root
#   .\package.ps1 -IncludeVfx        # + NVIDIA Video Effects runtime (SuperRes, ~1 GB)
#   .\package.ps1 -Zip               # + animation_imagee-<ver>.zip
#   .\package.ps1 -SkipBuild         # reuse the existing target\release binary
param(
    [string]$OutDir = 'dist',
    [switch]$IncludeModels,
    [switch]$IncludeVfx,
    [switch]$Zip,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $repo
try {
    if (-not $SkipBuild) {
        Write-Host '==> cargo build --release' -ForegroundColor Cyan
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    }

    $exe = Join-Path $repo 'target\release\animation_imagee.exe'
    if (-not (Test-Path $exe)) { throw "missing $exe (run without -SkipBuild)" }

    if (Test-Path $OutDir) { Remove-Item $OutDir -Recurse -Force }
    New-Item -ItemType Directory (Join-Path $OutDir 'libs') -Force | Out-Null

    # 1) main binary
    Copy-Item $exe $OutDir
    Write-Host '  + animation_imagee.exe'

    # 2) ONNX Runtime (loaded at runtime via ORT_DYLIB_PATH, else libs\ or exe dir)
    $ortFiles = @('onnxruntime.dll', 'onnxruntime_providers_shared.dll', 'DirectML.dll')
    foreach ($f in $ortFiles) {
        $src = Join-Path $repo "libs\$f"
        if (Test-Path $src) {
            Copy-Item $src (Join-Path $OutDir 'libs')
            Write-Host "  + libs\$f"
        } elseif ($f -ne 'DirectML.dll') {
            Write-Warning "libs\$f not found - the app cannot start without it"
        }
    }

    # 3) PE import table -> anything not provided by Windows itself gets copied
    function Get-ImportDlls([string]$Path) {
        $b = [IO.File]::ReadAllBytes($Path)
        $e = [BitConverter]::ToInt32($b, 0x3C)
        if ([BitConverter]::ToUInt32($b, $e) -ne 0x4550) { throw "not a PE file: $Path" }
        $coff = $e + 4
        $numSec = [BitConverter]::ToUInt16($b, $coff + 2)
        $optSize = [BitConverter]::ToUInt16($b, $coff + 16)
        $opt = $coff + 20
        $magic = [BitConverter]::ToUInt16($b, $opt)
        $dd = if ($magic -eq 0x20b) { $opt + 112 } else { $opt + 96 }
        $impRva = [BitConverter]::ToUInt32($b, $dd + 8)
        $secOff = $opt + $optSize
        $secs = @()
        for ($i = 0; $i -lt $numSec; $i++) {
            $o = $secOff + $i * 40
            $secs += [pscustomobject]@{
                VA      = [BitConverter]::ToUInt32($b, $o + 12)
                VSize   = [BitConverter]::ToUInt32($b, $o + 8)
                Raw     = [BitConverter]::ToUInt32($b, $o + 20)
                RawSize = [BitConverter]::ToUInt32($b, $o + 16)
            }
        }
        function Convert-Rva([uint32]$rva) {
            foreach ($s in $secs) {
                $span = [Math]::Max($s.VSize, $s.RawSize)
                if ($rva -ge $s.VA -and $rva -lt ($s.VA + $span)) { return $rva - $s.VA + $s.Raw }
            }
            return -1
        }
        $names = @()
        $off = Convert-Rva $impRva
        while ($off -gt 0) {
            $nameRva = [BitConverter]::ToUInt32($b, $off + 12)
            if ($nameRva -eq 0) { break }
            $p = Convert-Rva $nameRva
            $end = $p
            while ($b[$end] -ne 0) { $end++ }
            $names += [Text.Encoding]::ASCII.GetString($b, $p, $end - $p)
            $off += 20
        }
        return ($names | Sort-Object -Unique)
    }

    $sys32 = Join-Path $env:SystemRoot 'System32'
    $imports = Get-ImportDlls (Join-Path $OutDir 'animation_imagee.exe')
    $unresolved = @()
    foreach ($dll in $imports) {
        if ((Test-Path (Join-Path $OutDir $dll)) -or (Test-Path (Join-Path $OutDir "libs\$dll"))) { continue }
        # API sets (api-ms-*, ext-ms-*) are virtual names resolved by the OS loader
        if ($dll -match '^(api-ms-|ext-ms-)') { continue }
        # MSVC runtime is not part of Windows: always ship it next to the exe
        if ($dll -match '^(vcruntime|msvcp|concrt)') {
            $src = Join-Path $sys32 $dll
            if (Test-Path $src) {
                Copy-Item $src $OutDir
                Write-Host "  + $dll (MSVC runtime)"
            } else {
                $unresolved += "$dll (install VC++ 2015-2022 redistributable)"
            }
            continue
        }
        # Anything that already exists in System32 is an OS component (d3d11, dwmapi, ...)
        if (Test-Path (Join-Path $sys32 $dll)) { continue }
        $unresolved += $dll
    }

    # DirectML comes with Windows 10 1903+; warn when missing locally
    if (-not (Test-Path (Join-Path $OutDir 'libs\DirectML.dll')) -and
        -not (Test-Path (Join-Path $sys32 'DirectML.dll'))) {
        Write-Warning 'DirectML.dll not found - DirectML provider will fail on target PCs without it'
    }

    # 4) optional: models
    if ($IncludeModels) {
        $models = Get-ChildItem -Path $repo -Filter *.onnx -File
        foreach ($m in $models) {
            Copy-Item $m.FullName $OutDir
            Write-Host "  + $($m.Name)"
        }
        if ($models.Count -eq 0) { Write-Warning 'no *.onnx in repo root' }
    }

    # 5) optional: NVIDIA Video Effects runtime (SuperRes). Copied as .\nvvfx\,
    #    which the app also searches; redistributing it is subject to NVIDIA's terms.
    if ($IncludeVfx) {
        $vfx = 'C:\Program Files\NVIDIA Corporation\NVIDIA Video Effects'
        if (Test-Path $vfx) {
            Write-Host '  + nvvfx\ (NVIDIA Video Effects, ~1 GB)'
            $dst = Join-Path $OutDir 'nvvfx'
            New-Item -ItemType Directory $dst -Force | Out-Null
            Copy-Item (Join-Path $vfx '*.dll') $dst
            if (Test-Path (Join-Path $vfx 'models')) {
                Copy-Item (Join-Path $vfx 'models') $dst -Recurse
            }
        } else {
            Write-Warning "NVIDIA Video Effects not installed at $vfx - SuperRes will be unavailable"
        }
    }

    Write-Host ''
    Write-Host '== dist ==' -ForegroundColor Cyan
    $rootLen = (Resolve-Path $OutDir).Path.Length + 1
    $total = 0
    Get-ChildItem $OutDir -Recurse -File | Sort-Object FullName | ForEach-Object {
        $total += $_.Length
        '{0,10:N0} KB  {1}' -f ($_.Length / 1KB), $_.FullName.Substring($rootLen)
    }
    '{0,10:N1} MB  total' -f ($total / 1MB)

    if ($unresolved.Count -gt 0) {
        Write-Warning ("unresolved imports: " + ($unresolved -join ', '))
    }

    if ($Zip) {
        $ver = (Select-String -Path (Join-Path $repo 'Cargo.toml') -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
        $zipPath = Join-Path $repo "animation_imagee-$ver.zip"
        if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
        Compress-Archive -Path (Join-Path $OutDir '*') -DestinationPath $zipPath
        Write-Host "==> $zipPath" -ForegroundColor Cyan
    }

    Write-Host ''
    Write-Host "done: copy the whole '$OutDir' folder to the target PC and run animation_imagee.exe" -ForegroundColor Green
} finally {
    Pop-Location
}
