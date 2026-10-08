# Opt-in end-to-end check after publishing an updater-enabled release. Builds
# an older version with an isolated instance mutex and empty replay folder,
# then verifies a real startup downloads and installs the current release.
$ErrorActionPreference = 'Stop'
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$testRoot = Join-Path $workspace '.update-test'
$testDir = Join-Path $testRoot ([Guid]::NewGuid().ToString('N'))
$fixture = Join-Path $testDir 'fixture'
$install = Join-Path $testDir 'install'
$target = Join-Path $install 'valolysis.exe'
$previousAppData = $env:LOCALAPPDATA
$previousTargetDir = $env:CARGO_TARGET_DIR
$previousPortable = $env:VALOLYSIS_PORTABLE
$process = $null

try {
    New-Item -ItemType Directory -Path $fixture, $install -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $workspace 'src') -Destination $fixture -Recurse
    foreach ($name in @('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml')) {
        Copy-Item -LiteralPath (Join-Path $workspace $name) -Destination $fixture
    }
    $manifestPath = Join-Path $fixture 'Cargo.toml'
    $manifest = Get-Content $manifestPath -Raw
    $manifest = [regex]::Replace($manifest, '(?m)^version = "[^"]+"', 'version = "0.0.0"')
    [IO.File]::WriteAllText($manifestPath, $manifest, [Text.UTF8Encoding]::new($false))
    $platformPath = Join-Path $fixture 'src/platform.rs'
    $platform = (Get-Content $platformPath -Raw).Replace('ValolysisDesktop', ('ValolysisDesktopTest' + [Guid]::NewGuid().ToString('N')))
    [IO.File]::WriteAllText($platformPath, $platform, [Text.UTF8Encoding]::new($false))
    $env:CARGO_TARGET_DIR = Join-Path $testDir 'build'
    cargo build --release --manifest-path $manifestPath
    if ($LASTEXITCODE -ne 0) { throw 'Could not build the older fixture' }
    Copy-Item -LiteralPath (Join-Path $env:CARGO_TARGET_DIR 'release/valolysis.exe') -Destination $target
    $oldHash = (Get-FileHash $target -Algorithm SHA256).Hash.ToLowerInvariant()
    $latest = gh api repos/B1Fr0st/valolysis-desktop/releases/latest | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Could not read the published release' }
    $asset = $latest.assets | Where-Object { $_.name -eq 'valolysis-windows-x64.exe' }
    $expectedHash = $asset.digest.Substring(7)
    $env:LOCALAPPDATA = Join-Path $testDir 'AppData'
    # Run in place; the restarted app inherits this and does not install itself.
    $env:VALOLYSIS_PORTABLE = '1'
    $settingsDir = Join-Path $env:LOCALAPPDATA 'Valolysis'
    $demosDir = Join-Path $testDir 'EmptyDemos'
    New-Item -ItemType Directory -Path $settingsDir, $demosDir -Force | Out-Null
    $settings = @{ api = 'http://127.0.0.1:1'; auto_upload = $false; demos_dir = $demosDir } | ConvertTo-Json
    [IO.File]::WriteAllText((Join-Path $settingsDir 'settings.json'), $settings, [Text.UTF8Encoding]::new($false))
    $process = Start-Process $target -WindowStyle Hidden -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds(150)
    while ((Get-FileHash $target -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expectedHash) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'Startup update timed out' }
        if ($process.HasExited) {
            # The helper can replace the file just after its parent exits.
            Start-Sleep -Milliseconds 200
        }
        Start-Sleep -Milliseconds 100
    }
    if (!$process.WaitForExit(10000)) { throw 'The older app did not exit for the updater' }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    $logPath = Join-Path $settingsDir 'valolysis.log'
    do {
        $log = Get-Content -LiteralPath $logPath -Raw
        if ($log.Contains('desktop updated to ' + $latest.tag_name.Substring(1))) { break }
        if ([DateTime]::UtcNow -gt $deadline) { throw 'Update did not finish and restart' }
        Start-Sleep -Milliseconds 100
    } while ($true)
    if ($oldHash -eq $expectedHash) { throw 'Test did not exercise a version change' }
    Write-Output "PASS: startup version 0.0.0 automatically downloaded, verified, installed and launched $($latest.tag_name) from GitHub"
}
catch {
    $logPath = Join-Path $env:LOCALAPPDATA 'Valolysis/valolysis.log'
    if (Test-Path -LiteralPath $logPath) { Get-Content -LiteralPath $logPath -Tail 20 | Write-Host }
    throw
}
finally {
    $env:LOCALAPPDATA = $previousAppData
    $env:CARGO_TARGET_DIR = $previousTargetDir
    $env:VALOLYSIS_PORTABLE = $previousPortable
    if ($null -ne $process -and !$process.HasExited) { Stop-Process -Id $process.Id -ErrorAction SilentlyContinue }
    Get-Process | Where-Object {
        $_.Path -and $_.Path.StartsWith($testDir + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)
    } | Stop-Process -ErrorAction SilentlyContinue
    $resolvedTestDir = [IO.Path]::GetFullPath($testDir)
    if (!$resolvedTestDir.StartsWith($testRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing to clean a directory outside the integration-test workspace'
    }
    if (Test-Path -LiteralPath $resolvedTestDir) { Remove-Item -LiteralPath $resolvedTestDir -Recurse -Force }
}
