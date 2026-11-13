$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vcvars = & $vswhere -all -products * -sort -property installationPath |
    ForEach-Object { Join-Path $_ 'VC\Auxiliary\Build\vcvars64.bat' } |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $vcvars) { throw 'no Visual Studio install with vcvars64.bat' }
if (-not $env:VSCMD_VER) {
    cmd /c "`"$vcvars`" >nul && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($matches[1])" -Value $matches[2] }
    }
}
$extra = @("$env:USERPROFILE\.cargo\bin", 'C:\Program Files\CMake\bin', "$env:LOCALAPPDATA\Microsoft\WinGet\Links")
$env:Path = (@($extra | Where-Object { $env:Path -notlike "*$_*" }) + @($env:Path)) -join ';'
if (-not $env:CEF_PATH) { $env:CEF_PATH = 'C:\cf\cef' }
