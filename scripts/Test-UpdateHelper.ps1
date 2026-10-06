param(
    [string]$NewExecutable = (Join-Path $PSScriptRoot '../target/release/valolysis.exe')
)

# An opt-in executable-replacement integration check. It creates
# its own parent process and install directory and never stops the user's app.
$ErrorActionPreference = 'Stop'
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$testRoot = Join-Path $workspace '.update-test'
$testDir = Join-Path $testRoot ([Guid]::NewGuid().ToString('N'))
$stage = Join-Path $testDir ('.valolysis-update-' + [Guid]::NewGuid().ToString('N'))
$target = Join-Path $testDir 'Valolysis app.exe'
$helper = $null
$parent = $null
$previousAppData = $env:LOCALAPPDATA

try {
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    $env:LOCALAPPDATA = Join-Path $testDir 'AppData'
    $manifest = Get-Content (Join-Path $workspace 'Cargo.toml') -Raw
    $version = [regex]::Match($manifest, '(?m)^version = "([^"]+)"').Groups[1].Value
    Copy-Item -LiteralPath (Join-Path $env:WINDIR 'System32/where.exe') -Destination $target
    Copy-Item -LiteralPath $NewExecutable -Destination (Join-Path $stage 'helper.exe')
    $newHash = (Get-FileHash $NewExecutable -Algorithm SHA256).Hash.ToLowerInvariant()
    $oldHash = (Get-FileHash $target -Algorithm SHA256).Hash.ToLowerInvariant()
    $parent = Start-Process powershell.exe -ArgumentList '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30' -WindowStyle Hidden -PassThru
    $job = @{
        target = '\\?\' + $target
        parent_pid = $parent.Id
        version = $version
        sha256 = $newHash
        original_sha256 = $oldHash
    } | ConvertTo-Json
    [IO.File]::WriteAllText((Join-Path $stage 'update.json'), $job, [Text.UTF8Encoding]::new($false))
    $helper = Start-Process (Join-Path $stage 'helper.exe') -ArgumentList '--apply-update' -WindowStyle Hidden -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while (!(Test-Path (Join-Path $stage 'ready'))) {
        if ($helper.HasExited) { throw 'Helper exited before its readiness handshake' }
        if ([DateTime]::UtcNow -gt $deadline) { throw 'Helper readiness timed out' }
        Start-Sleep -Milliseconds 50
    }
    if ((Get-FileHash $target -Algorithm SHA256).Hash.ToLowerInvariant() -ne $oldHash) {
        throw 'The executable changed before the parent exited'
    }
    Stop-Process -Id $parent.Id
    if (!$helper.WaitForExit(15000) -or $helper.ExitCode -ne 0) {
        throw 'Helper did not successfully install the update'
    }
    if ((Get-FileHash $target -Algorithm SHA256).Hash.ToLowerInvariant() -ne $newHash) {
        throw 'The installed executable does not match the verified update'
    }
    if ((Get-FileHash (Join-Path $stage 'previous.exe') -Algorithm SHA256).Hash.ToLowerInvariant() -ne $oldHash) {
        throw 'The backup does not match the original executable'
    }
    if (!(Test-Path (Join-Path $stage 'complete'))) { throw 'Missing completion receipt' }
    $versionProcess = Start-Process $target -ArgumentList '--version' -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $testDir 'version.txt')
    if (!$versionProcess.WaitForExit(10000)) { Stop-Process -Id $versionProcess.Id; throw 'Version check timed out' }
    if ((Get-Content (Join-Path $testDir 'version.txt') -Raw).Trim() -ne "valolysis $version") {
        throw 'Updated binary reports the wrong version'
    }
    Write-Output "PASS: parent-exit handoff, real executable replacement, old-binary backup, restart launch, and version $version"
}
catch {
    $logPath = Join-Path $env:LOCALAPPDATA 'Valolysis/valolysis.log'
    if (Test-Path -LiteralPath $logPath) { Get-Content -LiteralPath $logPath -Tail 12 | Write-Host }
    throw
}
finally {
    $env:LOCALAPPDATA = $previousAppData
    foreach ($process in @($parent, $helper)) {
        if ($null -ne $process -and !$process.HasExited) { Stop-Process -Id $process.Id -ErrorAction SilentlyContinue }
    }
    Get-Process | Where-Object { $_.Path -eq $target } | Stop-Process -ErrorAction SilentlyContinue
    $resolvedTestDir = [IO.Path]::GetFullPath($testDir)
    if (!$resolvedTestDir.StartsWith($testRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing to clean a directory outside the integration-test workspace'
    }
    if (Test-Path -LiteralPath $resolvedTestDir) { Remove-Item -LiteralPath $resolvedTestDir -Recurse -Force }
}
