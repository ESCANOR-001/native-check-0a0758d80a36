param([Parameter(Mandatory = $true)][string]$BundleDir)
$ErrorActionPreference = 'Stop'
$installer = @(Get-ChildItem (Join-Path $BundleDir 'nsis') -Filter '*-setup.exe')
if ($installer.Count -ne 1) { throw 'Expected one Windows NSIS installer' }
$destination = Join-Path $env:SystemDrive 'remodex-desktop-qa'
if (Test-Path $destination) { throw 'The isolated desktop install directory is occupied' }
$process = Start-Process -FilePath $installer[0].FullName -ArgumentList @('/S', "/D=$destination") -WindowStyle Hidden -Wait -PassThru
if ($process.ExitCode -ne 0) { throw "Desktop installer exited with $($process.ExitCode)" }
try {
    if (-not (Test-Path "$destination\remodex.exe" -PathType Leaf)) { throw 'Installed application missing' }
    if (-not (Test-Path "$destination\uninstall.exe" -PathType Leaf)) { throw 'Installed uninstaller missing' }
    Write-Host 'Windows installed the Remodex application into its isolated QA directory.'
} finally {
    Get-Process remodex -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq "$destination\remodex.exe" } | Stop-Process
    if (Test-Path "$destination\uninstall.exe" -PathType Leaf) {
        $removed = Start-Process -FilePath "$destination\uninstall.exe" -ArgumentList '/S' -WindowStyle Hidden -Wait -PassThru
        if ($removed.ExitCode -ne 0) { throw "Desktop uninstaller exited with $($removed.ExitCode)" }
    }
}
$waitUntil = (Get-Date).AddSeconds(30)
while ((Test-Path "$destination\remodex.exe") -and (Get-Date) -lt $waitUntil) { Start-Sleep -Seconds 1 }
if (Test-Path "$destination\remodex.exe") { throw 'Installed application remained after uninstall' }
Write-Host 'Windows removed the isolated desktop application.'
