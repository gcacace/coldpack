$ErrorActionPreference = "Stop"

$repo = "gcacace/coldpack"
$target = "x86_64-pc-windows-msvc"

Write-Host "Detected platform: $target"

Write-Host "Fetching latest release..."
$release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
$tag = $release.tag_name
Write-Host "Latest release: $tag"

$archive = "coldpack-$tag-$target.zip"
$url = "https://github.com/$repo/releases/download/$tag/$archive"

$tmpDir = Join-Path $env:TEMP "coldpack-install"
if (Test-Path $tmpDir) { Remove-Item -Recurse -Force $tmpDir }
New-Item -ItemType Directory -Path $tmpDir | Out-Null

Write-Host "Downloading $archive..."
Invoke-WebRequest -Uri $url -OutFile (Join-Path $tmpDir $archive)

Expand-Archive -Path (Join-Path $tmpDir $archive) -DestinationPath $tmpDir -Force

$installDir = Join-Path $env:LOCALAPPDATA "coldpack"
if (-not (Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir | Out-Null
}
Move-Item -Force (Join-Path $tmpDir "coldpack.exe") (Join-Path $installDir "coldpack.exe")

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$installDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$installDir", "User")
    $env:Path = "$env:Path;$installDir"
    Write-Host "Added $installDir to user PATH"
}

Remove-Item -Recurse -Force $tmpDir

$exe = Join-Path $installDir "coldpack.exe"
Write-Host "Installed coldpack to $exe"
& $exe --version
