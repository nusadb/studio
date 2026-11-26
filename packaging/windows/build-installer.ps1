# Build the Windows release, a portable .zip and the setup .exe (NSIS) on Windows.
#   powershell -ExecutionPolicy Bypass -File packaging\windows\build-installer.ps1   -> dist\
# Needs NSIS 3 for the installer (winget install NSIS.NSIS); without it only the .zip is built.
$ErrorActionPreference = "Stop"
$Root = Resolve-Path "$PSScriptRoot\..\.."
Set-Location $Root
$Version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches.Groups[1].Value
$Dist = Join-Path $Root "dist"
New-Item -ItemType Directory -Force $Dist | Out-Null

cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
$Exe = Join-Path $Root "target\release\nusadb-studio.exe"

$Portable = Join-Path $Dist "nusadb-studio-$Version-windows-x64"
Remove-Item -Recurse -Force $Portable -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$Portable\licenses" | Out-Null
Copy-Item $Exe, README.md $Portable
Copy-Item ui\fonts\OFL-*.txt "$Portable\licenses"
Compress-Archive -Force -Path $Portable -DestinationPath "$Portable.zip"
Remove-Item -Recurse -Force $Portable

$Makensis = @("${env:ProgramFiles(x86)}\NSIS\makensis.exe", "$env:ProgramFiles\NSIS\makensis.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if ($Makensis) {
    Push-Location packaging\windows
    & $Makensis /V2 "/DVERSION=$Version" "/DEXE=$Exe" "/DOUTDIR=$Dist" nusadb-studio.nsi
    $code = $LASTEXITCODE
    Pop-Location
    if ($code -ne 0) { throw "makensis failed" }
} else {
    Write-Warning "NSIS not found: only the portable .zip was built (winget install NSIS.NSIS)."
}
Get-ChildItem $Dist
