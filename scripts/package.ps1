# Build a Windows release zip: dist\ambersight-<version>-windows-x86_64.zip
# with the program, an example config, the README and install.cmd.
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches.Groups[1].Value
$name = "ambersight-$version-windows-x86_64"
# Keep build-machine paths (user names) out of the program.
$env:RUSTFLAGS = "--remap-path-prefix=$env:USERPROFILE=~ --remap-path-prefix=$((Get-Location).Path)=."
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "build failed" }
$out = Join-Path "dist" $name
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item target\release\ambersight.exe, config.example.toml $out
# The README without its pictures (those files are not in the package; it links nowhere that needs an account).
$readme = (Get-Content README.md -Raw -Encoding UTF8) -replace '(?ms)^<p align="center">\r?\n.*?^</p>\r?\n', '' -replace '(?ms)^## Screenshots.*?(?=^## Install)', '' -replace '(?m)^[^\r\n]*docs/images[^\r\n]*\r?\n', '' -replace '(?m)^More screenshots:[^\r\n]*\r?\n', ''  -replace '\[([^\]]*)\]\(docs/[^)]*\)', '$1'
[IO.File]::WriteAllText((Join-Path (Resolve-Path $out) "README.md"), $readme, (New-Object Text.UTF8Encoding $false))
# The AI runtime's libraries (DirectML.dll and any other DLL the build placed
# next to the program): without them object detection cannot start.
Get-ChildItem target\release\*.dll -ErrorAction SilentlyContinue | Copy-Item -Destination $out
if (-not (Test-Path (Join-Path $out "DirectML.dll"))) { throw "DirectML.dll is missing from target\release: the package would have no graphics-card AI" }
# The Microsoft Visual C++ runtime the AI library needs (msvcp140*.dll,
# vcruntime140*.dll), next to the program: a Windows without the Visual C++
# Redistributable installed would not start AmberSight at all (missing DLL).
# Microsoft allows shipping these files with an app ("app-local"); they come
# from Visual Studio's own Redist folder.
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -property installationPath
$crt = Get-ChildItem (Join-Path $vs "VC\Redist\MSVC\*\x64\Microsoft.VC14*.CRT") -Directory | Sort-Object FullName -Descending | Select-Object -First 1
if (-not $crt) { throw "The Visual C++ runtime (VC\Redist\MSVC\...\x64\Microsoft.VC14x.CRT) was not found under $vs" }
foreach ($dll in "msvcp140.dll", "msvcp140_1.dll", "vcruntime140.dll", "vcruntime140_1.dll") {
  $f = Join-Path $crt.FullName $dll
  if (-not (Test-Path $f)) { throw "$f is missing" }
  Copy-Item $f $out
}
if (Test-Path LICENSE) { Copy-Item LICENSE $out }
if (Test-Path THIRD-PARTY-NOTICES.txt) { Copy-Item THIRD-PARTY-NOTICES.txt $out }
@'
@echo off
rem Installs AmberSight from this folder (run as Administrator): the Windows
rem service (starts at boot), a firewall rule for port 8090 and shortcuts.
rem Settings (config.toml) and recordings stay in this folder.
rem No ffmpeg on this computer? AmberSight downloads it by itself on first start (about 105 MB, one time).
cd /d "%~dp0"
ambersight.exe setup --here
pause
'@ | Set-Content -Encoding ASCII (Join-Path $out "install.cmd")
$zip = "dist\$name.zip"
if (Test-Path $zip) { Remove-Item $zip }
# tar.exe (built into Windows 10 and 11) writes standard "/" paths;
# Compress-Archive writes "\" paths, which break unpacking on Mac and Linux.
tar.exe -a -c -f $zip -C dist $name
if ($LASTEXITCODE -ne 0) { throw "zip failed" }
Write-Output $zip
