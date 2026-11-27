# Usage: bundle-windows.ps1 <target-triple> <arch-label>
# Package the release binary twice from one staged payload:
#   dist/ntty7-<version>-windows-<arch>.zip        portable (unzip anywhere)
#   dist/ntty7-<version>-windows-<arch>-setup.exe  Inno Setup installer
#     (Program Files or per-user, Start Menu shortcut, "Apps" uninstall entry)
#
# Fonts are embedded via include_bytes! and the app icon is compiled into the
# executable as a resource (see build.rs). So the payload is ntty7.exe, the
# bundled ConPTY pair (loaded by the DLL search path — see below) and the
# license/readme. Windows release artifacts are intentionally unsigned. The
# in-app updater verifies the published SHA-256 checksum and PE file version
# before and after waiting for the GUI to exit.
$ErrorActionPreference = 'Stop'

$Target = $args[0]
$Arch   = $args[1]
$Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
# Inno accepts the full semantic version for AppVersion, but the PE version
# resource only accepts numeric components. Keep both values so Nightly and
# other prerelease builds retain their display version without breaking ISCC.
$VersionCore = ($Version -split '[-+]', 2)[0]
$VersionInfoVersion = "${VersionCore}.0"
$Name  = "ntty7-$Version-windows-$Arch"
$Stage = "dist/$Name"
$PackageUpdater = $env:TTY7_PACKAGE_UPDATE_HELPER -ne '0'

Remove-Item -Recurse -Force dist -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null

Copy-Item "target/$Target/release/ntty7.exe" "$Stage/ntty7.exe"
Copy-Item LICENSE "$Stage/LICENSE.txt"
Copy-Item README.md "$Stage/README.md"

# Microsoft's redistributable ConPTY, which `portable-pty` prefers over the
# in-box conhost when it sits beside the running executable. The daemon runs
# inside ntty7.exe, so beside that binary is exactly here. The in-box host
# swallows a pane's OSC 11 background query (#345); this pair forwards it. The
# two files are one unit, so a missing half is an error rather than a warning.
# See assets/windows/conpty/README.md.
$ConptyArch = @{ 'x86_64-pc-windows-msvc' = 'x64'; 'aarch64-pc-windows-msvc' = 'arm64' }[$Target]
if (-not $ConptyArch) { throw "no vendored ConPTY for $Target - see assets/windows/conpty/README.md" }
foreach ($File in 'conpty.dll', 'OpenConsole.exe') {
    $Source = "assets/windows/conpty/$ConptyArch/$File"
    if (-not (Test-Path $Source)) { throw "missing bundled ConPTY file: $Source" }
    Copy-Item $Source "$Stage/$File"
}
Copy-Item "assets/windows/conpty/LICENSE.txt" "$Stage/LICENSE-ConPTY.txt"

# The marker tells the in-app updater which of the two Windows layouts it is
# running from, and therefore which release asset can replace it. It says
# nothing about where updates come from: that is always the latest stable
# release. The Inno payload gets the mutually exclusive marker below.
if ($PackageUpdater) {
    Set-Content -Path "$Stage/.tty7-portable" -Value 'portable-v1' -NoNewline -Encoding ascii
}
Compress-Archive -Path "$Stage/*" -DestinationPath "dist/$Name.zip" -Force
if ($PackageUpdater) {
    # The Inno payload must never retain the mutually exclusive portable marker.
    Remove-Item -LiteralPath "$Stage/.tty7-portable" -Force
    Set-Content -Path "$Stage/.tty7-inno-install" -Value 'inno-v1' -NoNewline -Encoding ascii
}

# Inno's architecture identifier for this payload. `x64compatible` also admits
# ARM64 Windows, which runs the x64 build under emulation — so an ARM64 machine
# can take either installer, and both upgrade the same install (one AppId).
$InstallArch = @{
    'x86_64-pc-windows-msvc'  = 'x64compatible'
    'aarch64-pc-windows-msvc' = 'arm64'
}[$Target]
if (-not $InstallArch) { throw "no Inno architecture known for $Target" }

# Installer, built from the same staged payload. ISCC is on PATH on GitHub's
# windows-latest image; fall back to the default install location.
$Iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
if (-not $Iscc) { $Iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" }
& $Iscc `
    "/DAppVersion=$Version" `
    "/DVersionInfoVersion=$VersionInfoVersion" `
    "/DStageDir=$((Resolve-Path $Stage).Path)" `
    "/DOutputDir=$((Resolve-Path dist).Path)" `
    "/DOutputName=$Name-setup" `
    "/DArchitectures=$InstallArch" `
    .github/scripts/windows-installer.iss
if ($LASTEXITCODE -ne 0) { throw "ISCC exited with $LASTEXITCODE" }

# The staging directory stays. It is what ISCC compiled the installer from, and
# reading the compiled setup.exe back would need innoextract, which the runners
# do not carry — so `verify-windows-package.ps1` reads the payload here instead.
# It never reaches the release: both workflows upload named file globs
# (*.zip, *-setup.exe, …), and a directory matches none of them.
Write-Host "OK dist/$Name.zip"
Write-Host "OK dist/$Name-setup.exe"
