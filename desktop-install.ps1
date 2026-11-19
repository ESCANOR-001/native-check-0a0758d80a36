param([Parameter(Mandatory = $true)][string]$BundleDir)
$ErrorActionPreference = 'Stop'
$installer = @(Get-ChildItem (Join-Path $BundleDir 'nsis') -Filter '*-setup.exe')
if ($installer.Count -ne 1) { throw 'Expected one Windows NSIS installer' }
$destination = Join-Path $env:SystemDrive 'remodex-desktop-qa'
if (Test-Path $destination) { throw 'The isolated desktop install directory is occupied' }
# The installer starts the desktop shell. Wait on the installer PID only,
# since Start-Process -Wait also waits on that long-running child's tree.
$process = Start-Process -FilePath $installer[0].FullName -ArgumentList @('/S', "/D=$destination") -WindowStyle Hidden -PassThru
if (-not $process.WaitForExit(90000)) {
    Stop-Process -Id $process.Id -ErrorAction SilentlyContinue
    throw 'The desktop installer did not finish within 90 seconds'
}
if ($process.ExitCode -ne 0) { throw "Desktop installer exited with $($process.ExitCode)" }
try {
    if (-not (Test-Path "$destination\remodex.exe" -PathType Leaf)) { throw 'Installed application missing' }
    if (-not (Test-Path "$destination\uninstall.exe" -PathType Leaf)) { throw 'Installed uninstaller missing' }
    Write-Host 'Windows installed the Remodex application into its isolated QA directory.'
} finally {
    Get-Process remodex -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq "$destination\remodex.exe" } | Stop-Process
    if (Test-Path "$destination\uninstall.exe" -PathType Leaf) {
        $removed = Start-Process -FilePath "$destination\uninstall.exe" -ArgumentList '/S' -WindowStyle Hidden -PassThru
        if (-not $removed.WaitForExit(90000)) {
            Stop-Process -Id $removed.Id -ErrorAction SilentlyContinue
            throw 'The desktop uninstaller did not finish within 90 seconds'
        }
        if ($removed.ExitCode -ne 0) { throw "Desktop uninstaller exited with $($removed.ExitCode)" }
    }
}
$waitUntil = (Get-Date).AddSeconds(30)
while ((Test-Path "$destination\remodex.exe") -and (Get-Date) -lt $waitUntil) { Start-Sleep -Seconds 1 }
if (Test-Path "$destination\remodex.exe") { throw 'Installed application remained after uninstall' }
Write-Host 'Windows removed the isolated desktop application.'
