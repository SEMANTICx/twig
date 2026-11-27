$ErrorActionPreference = "Stop"

$Repo = if ($env:TWIG_REPO) { $env:TWIG_REPO } else { "SEMANTICx/twig-dist" }
$InstallDir = if ($env:TWIG_INSTALL_DIR) { $env:TWIG_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "twig\bin" }
$Target = "x86_64-pc-windows-msvc"

if ($env:TWIG_VERSION) {
    $Tag = $env:TWIG_VERSION
} else {
    $Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
    $Tag = $Release.tag_name
}

if (-not $Tag) {
    throw "Could not resolve latest release tag from $Repo. Set `$env:TWIG_VERSION = 'v0.2.0' to pin explicitly."
}

$Pkg = "twig-$Tag-$Target"
$BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("twig-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ZipPath = Join-Path $TmpDir "$Pkg.zip"
    $ShaPath = Join-Path $TmpDir "$Pkg.zip.sha256"

    Write-Host "Downloading $Pkg.zip..."
    Invoke-WebRequest -Uri "$BaseUrl/$Pkg.zip" -OutFile $ZipPath
    Invoke-WebRequest -Uri "$BaseUrl/$Pkg.zip.sha256" -OutFile $ShaPath

    $Expected = (Get-Content $ShaPath -Raw).Trim().Split()[0].ToLower()
    $Actual = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLower()
    if ($Expected -ne $Actual) {
        throw "SHA-256 mismatch: expected $Expected, got $Actual"
    }

    Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item (Join-Path $TmpDir "$Pkg\twig.exe") (Join-Path $InstallDir "twig.exe") -Force

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not $UserPath) { $UserPath = "" }
    if ($UserPath.Split(';') -notcontains $InstallDir) {
        $NewPath = if ($UserPath) { "$UserPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        Write-Host "Added $InstallDir to User PATH (restart terminal to pick up)."
    }
    Write-Host "Installed twig $Tag to $(Join-Path $InstallDir 'twig.exe')"
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
