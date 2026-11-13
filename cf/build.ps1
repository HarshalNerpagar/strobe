param([switch]$SkipPackage, [switch]$Test, [string]$Dist)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'env.ps1')
Set-Location $repo

function Invoke-Native([string]$CommandLine) {
    cmd /c "$CommandLine 2>&1" | Out-Host
    $LASTEXITCODE
}

$toolchain = (Select-String -Path rust-toolchain.toml -Pattern 'channel\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
if (Invoke-Native "rustup toolchain install $toolchain --profile minimal") { throw 'rustup toolchain install failed' }

$sw = [Diagnostics.Stopwatch]::StartNew()
if (Invoke-Native 'cargo build --release -p vrf-cli') { throw 'cargo build failed' }
'cargo build: {0:N0} s' -f $sw.Elapsed.TotalSeconds | Out-Host

if ($Test) {
    foreach ($t in '-p vrf --lib', '-p vrf-encode --lib', '-p vrf-bench --lib --bins --test corpus --test score --test run --test versions', '-p vrf-cli --bin strobe') {
        $code = Invoke-Native "cargo test --release $t"
        "cargo test --release $t => exit $code" | Out-Host
    }
}

if ($SkipPackage) { return }

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
if (-not $Dist) { $Dist = Join-Path $repo "dist\cf-$version-src" }
$release = Join-Path $repo 'target\release'
if (Test-Path $Dist) { Remove-Item -Recurse -Force $Dist }
New-Item -ItemType Directory -Force (Join-Path $Dist 'locales') | Out-Null

$required = 'strobe.exe', 'libcef.dll', 'chrome_elf.dll', 'icudtl.dat', 'resources.pak', 'v8_context_snapshot.bin'
$optional = 'chrome_100_percent.pak', 'chrome_200_percent.pak', 'd3dcompiler_47.dll', 'dxcompiler.dll', 'dxil.dll',
            'vk_swiftshader.dll', 'vk_swiftshader_icd.json', 'vulkan-1.dll'
foreach ($f in $required) { Copy-Item (Join-Path $release $f) $Dist }
foreach ($f in $optional) { $p = Join-Path $release $f; if (Test-Path $p) { Copy-Item $p $Dist } }
Copy-Item (Join-Path $release 'locales\*.pak') (Join-Path $Dist 'locales')

$cefArchive = Get-ChildItem -Path $env:CEF_PATH -Recurse -Filter archive.json -ErrorAction SilentlyContinue | Select-Object -First 1
$info = [ordered]@{
    commit    = (& git rev-parse HEAD)
    branch    = (& git rev-parse --abbrev-ref HEAD)
    dirty     = [bool](& git status --porcelain --untracked-files=no)
    built     = (Get-Date).ToString('s')
    rustc     = (& rustc --version)
    msvc      = $env:VCToolsVersion
    sdk       = $env:WindowsSDKVersion
    cef       = if ($cefArchive) { (Get-Content $cefArchive.FullName | ConvertFrom-Json).name } else { $null }
    sha256    = (Get-FileHash (Join-Path $Dist 'strobe.exe') -Algorithm SHA256).Hash
}
$info | ConvertTo-Json | Set-Content (Join-Path $Dist 'BUILDINFO.json') -Encoding ASCII
"packaged: $Dist" | Out-Host
