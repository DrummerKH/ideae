# Installs the latest release of ideae for Windows (x64) into
# %LOCALAPPDATA%\Programs\ideae, with a Start menu shortcut:
#
#   irm https://github.com/scillarium/ideae/releases/latest/download/install.ps1 | iex
#
# Run again to update; the app also updates itself. `ideae.exe --uninstall` removes it.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'scillarium/ideae'
$tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
$dir = Join-Path $env:LOCALAPPDATA 'Programs\ideae'
New-Item -ItemType Directory -Force $dir | Out-Null
$exe = Join-Path $dir 'ideae.exe'
$old = Join-Path $dir 'ideae.old.exe'

Invoke-WebRequest "https://github.com/$repo/releases/download/$tag/ideae-$tag-windows-x86_64.exe" -OutFile "$exe.download" -UseBasicParsing
# A running ideae.exe can be renamed but not overwritten: put aside, as the app's own update does.
Remove-Item -Force $old -ErrorAction SilentlyContinue
if (Test-Path $exe) { Move-Item -Force $exe $old }
Move-Item -Force "$exe.download" $exe
$made = Start-Process -FilePath $exe -ArgumentList '--install' -Wait -PassThru
if ($made.ExitCode -ne 0) { throw "ideae --install failed" }
Write-Host "ideae $tag installed: in the Start menu, and it can be pinned to the taskbar."
