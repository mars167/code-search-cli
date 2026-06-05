param(
    [string]$Version = $env:CODE_SEARCH_VERSION,
    [string]$Repo = $env:CODE_SEARCH_REPO,
    [string]$InstallDir = $env:CODE_SEARCH_INSTALL_DIR,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = "latest"
}
if ([string]::IsNullOrWhiteSpace($Repo)) {
    $Repo = "mars167/code-search-cli"
}
if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $localAppData = $env:LOCALAPPDATA
    if ([string]::IsNullOrWhiteSpace($localAppData)) {
        $localAppData = Join-Path $HOME ".local"
    }
    $InstallDir = Join-Path $localAppData "Programs\code-search-cli\bin"
}
if ($env:CODE_SEARCH_DRY_RUN -eq "1") {
    $DryRun = $true
}

function Get-CodeSearchArchitecture {
    $arch = $env:CODE_SEARCH_ARCH
    if ([string]::IsNullOrWhiteSpace($arch)) {
        $arch = $env:PROCESSOR_ARCHITEW6432
    }
    if ([string]::IsNullOrWhiteSpace($arch)) {
        $arch = $env:PROCESSOR_ARCHITECTURE
    }
    if ([string]::IsNullOrWhiteSpace($arch)) {
        $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    }

    switch -Regex ($arch) {
        "^(X64|x86_64|amd64)$" { return "amd64" }
        "^(AMD64)$" { return "amd64" }
        "^(Arm64|ARM64|arm64|aarch64)$" { return "arm64" }
        default { throw "Unsupported architecture: $arch" }
    }
}

$assetArch = Get-CodeSearchArchitecture
$asset = "code-search-windows-$assetArch.exe.zip"
if ($Version -eq "latest") {
    $baseUrl = "https://github.com/$Repo/releases/latest/download"
} else {
    $baseUrl = "https://github.com/$Repo/releases/download/$Version"
}

if ($DryRun) {
    Write-Output "repo=$Repo"
    Write-Output "version=$Version"
    Write-Output "asset=$asset"
    Write-Output "install_dir=$InstallDir"
    Write-Output "url=$baseUrl/$asset"
    return
}

$tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("code-search-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmpDir | Out-Null

try {
    $assetPath = Join-Path $tmpDir $asset
    $checksumsPath = Join-Path $tmpDir "SHA256SUMS"

    Write-Output "Downloading $asset..."
    Invoke-WebRequest -Uri "$baseUrl/$asset" -OutFile $assetPath
    Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile $checksumsPath

    $expected = $null
    foreach ($line in Get-Content $checksumsPath) {
        $parts = $line -split "\s+"
        if ($parts.Length -ge 2 -and $parts[1] -eq $asset) {
            $expected = $parts[0].ToLowerInvariant()
            break
        }
    }
    if ([string]::IsNullOrWhiteSpace($expected)) {
        throw "Checksum for $asset was not found in SHA256SUMS."
    }

    $actual = (Get-FileHash -Algorithm SHA256 -Path $assetPath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "Checksum mismatch for $asset. Expected $expected, got $actual."
    }

    $extractDir = Join-Path $tmpDir "extract"
    Expand-Archive -Path $assetPath -DestinationPath $extractDir -Force
    $exePath = Join-Path $extractDir "code-search.exe"
    if (-not (Test-Path $exePath)) {
        throw "Release archive did not contain code-search.exe."
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -Path $exePath -Destination (Join-Path $InstallDir "code-search.exe") -Force

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $pathParts = @()
    if (-not [string]::IsNullOrWhiteSpace($userPath)) {
        $pathParts = $userPath -split ";"
    }
    if ($pathParts -notcontains $InstallDir) {
        $newUserPath = if ([string]::IsNullOrWhiteSpace($userPath)) { $InstallDir } else { "$userPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")
    }
    if (($env:Path -split ";") -notcontains $InstallDir) {
        $env:Path = "$env:Path;$InstallDir"
    }

    Write-Output "Installed code-search to $(Join-Path $InstallDir 'code-search.exe')"
    Write-Output "Restart your terminal if code-search is not found immediately."
}
finally {
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
}
