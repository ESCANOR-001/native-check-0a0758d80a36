param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$StartupHelper
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$qa = Join-Path $env:SystemDrive "rmx-sign-in-$env:GITHUB_RUN_ATTEMPT"
$qaUser = 'RemodexQATest'
$bootstrap = 'Remodex QA first-session onboarding'
$firstLogin = $null
$desk = $null
$productTask = $null
$createdUser = $false
$rdp = 'C:\msys64\ucrt64\bin\wfreerdp.exe'
$sys = "$env:WINDIR\System32"
$python = (Get-Command python -CommandType Application | Select-Object -First 1).Source

function Wait-ForCondition($Label, [int]$Seconds, [scriptblock]$Check) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        $result = & $Check
        if ($result) { return $result }
        Start-Sleep -Milliseconds 900
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Label"
}
function Get-QaSession {
    $line = (& "$sys\query.exe" session 2>$null | Select-String -Pattern "\b$qaUser\s+(\d+)\s+Active\b").Matches
    if ($line) { return [int]$line[0].Groups[1].Value }
    return $null
}
function Get-QaHealth([int]$Port) {
    try {
        $response = Invoke-RestMethod -NoProxy -Uri "http://127.0.0.1:$Port/healthz" -TimeoutSec 2
        return ($response.status -ceq 'running' -and $response.service -ceq 'remodex-native-connect-experiment')
    } catch { return $false }
}
function Open-QaDesktop {
    # The runner already offers RDP. Use only this VM's loopback listener and
    # trust its first certificate for the lifetime of this disposable client.
    $client = Start-Process -FilePath $rdp -ArgumentList @(
        '/v:127.0.0.1', "/u:$qaUser", "/d:$env:COMPUTERNAME", "/p:$password",
        '/cert:tofu', '/size:800x600', '/log-level:ERROR',
        '/network:lan'
    ) -WindowStyle Hidden -RedirectStandardOutput (Join-Path $qa 'desktop.out') `
        -RedirectStandardError (Join-Path $qa 'desktop.err') -PassThru
    return $client
}
function Refresh-QaRegistration($TaskName) {
    $entry = Get-ScheduledTask -TaskName $TaskName
    $info = Get-ScheduledTaskInfo -TaskName $TaskName
    return [PSCustomObject]@{ State = $entry.State; LastRun = $info.LastRunTime; LastResult = $info.LastTaskResult }
}
function Close-QaDesktop {
    $session = Get-QaSession
    if ($null -ne $session) {
        & "$sys\logoff.exe" $session
        if ($LASTEXITCODE -ne 0) { throw 'Could not log off the isolated test user' }
        $null = Wait-ForCondition 'the test account to sign out completely' 30 {
            -not ((& "$sys\query.exe" session 2>$null | Out-String) -match "\b$qaUser\s+\d+\s+")
        }
    }
    if ($null -ne $script:desk) {
        if (-not $script:desk.HasExited) { Stop-Process -Id $script:desk.Id }
        $script:desk.Dispose()
        $script:desk = $null
    }
}

try {
    if (Get-LocalUser -Name $qaUser -ErrorAction SilentlyContinue) { throw 'Unexpected existing QA account' }
    if ((Get-ItemPropertyValue 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server' fDenyTSConnections) -ne 0) {
        throw 'The runner does not have the required Remote Desktop listener'
    }
    if (-not (Test-Path $rdp)) { throw 'The FreeRDP client is missing' }
    New-Item -ItemType Directory -Path $qa | Out-Null
    $password = 'RmX#7' + [Guid]::NewGuid().ToString('N')
    Write-Host "::add-mask::$password"
    $account = New-LocalUser -Name $qaUser -Password (ConvertTo-SecureString $password -AsPlainText -Force)
    $createdUser = $true
    $rdpGroup = Get-LocalGroup -SID 'S-1-5-32-555'
    Add-LocalGroupMember -Group $rdpGroup.Name -Member $account
    $userSpec = "$env:COMPUTERNAME\$qaUser"
    & "$sys\icacls.exe" $qa /grant "${userSpec}:(OI)(CI)M" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Could not grant the test user access to its fixture' }

    Copy-Item $Binary (Join-Path $qa 'rmx.exe')
    Copy-Item $StartupHelper (Join-Path $qa 'remodex-startup.exe')
    Copy-Item (Join-Path $PSScriptRoot 'onboard_pty.py') $qa
    & $python -m pip install --disable-pip-version-check --only-binary ':all:' --no-deps --target "$qa\python-packages" 'pywinpty==3.1.2'
    if ($LASTEXITCODE -ne 0) { throw 'Could not prepare Windows pseudoterminal' }
    # The test starts this one-shot task only for the first session.
    # It has no trigger, so it cannot run onboarding at the next sign-in.
    $port = 27130
    $launchScript = @"
[Environment]::SetEnvironmentVariable('PYTHONPATH', '$qa\python-packages')
& '$python' -u '$qa\onboard_pty.py' '$qa' $port *> '$qa\harness.log'
exit `$LASTEXITCODE
"@
    Set-Content -Path "$qa\bootstrap.ps1" -Value $launchScript -Encoding UTF8
    $action = New-ScheduledTaskAction -Execute "$sys\WindowsPowerShell\v1.0\powershell.exe" `
        -Argument "-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -File `"$qa\bootstrap.ps1`""
    $principal = New-ScheduledTaskPrincipal -UserId $userSpec -LogonType Interactive -RunLevel Limited
    Register-ScheduledTask -TaskName $bootstrap -Action $action -Principal $principal | Out-Null
    & "$sys\wevtutil.exe" sl 'Microsoft-Windows-TaskScheduler/Operational' '/e:true'

    $desk = Open-QaDesktop
    $firstLogin = Wait-ForCondition 'first interactive Windows sign-in' 70 {
        if ($desk.HasExited) { throw "Remote Desktop client exited at first sign-in with code $($desk.ExitCode)" }
        Get-QaSession
    }
    Write-Host "Windows first sign-in reached interactive session $firstLogin."
    Start-ScheduledTask -TaskName $bootstrap
    try {
        $null = Wait-ForCondition 'Yes at the real rmx onboard prompt' 130 { Test-Path "$qa\onboard-result.json" }
    } catch {
        foreach ($file in @('harness.log', 'onboard.log')) { if (Test-Path "$qa\$file") { Get-Content "$qa\$file" -Tail 55 } }
        throw
    }
    $initial = Get-Content "$qa\onboard-result.json" -Raw | ConvertFrom-Json
    if (-not $initial.answeredYes -or -not (Get-QaHealth ($port+5))) { throw 'Initial onboarding was not ready' }
    $records = @(Get-ScheduledTask -TaskName 'remodex-native-*')
    if ($records.Count -ne 1) { throw 'Expected exactly one Remodex login registration' }
    $productTask = $records[0].TaskName
    [xml]$definition = Export-ScheduledTask -TaskName $productTask
    if ($null -eq $definition.Task.Triggers.LogonTrigger -or $definition.Task.Triggers.LogonTrigger.Enabled -ne 'true') {
        throw 'The Remodex trigger is missing or disabled'
    }
    $before = Refresh-QaRegistration $productTask
    Unregister-ScheduledTask -TaskName $bootstrap -Confirm:$false
    Write-Host 'Interactive Yes registered one enabled Windows logon trigger.'

    Close-QaDesktop
    $null = Wait-ForCondition 'the original Windows server to go offline after sign-out' 50 {
        -not (Get-QaHealth ($port+5)) -and -not (Get-Process -Id $initial.initialPid -ErrorAction SilentlyContinue)
    }
    Write-Host 'Windows signed out and the original server exited.'

    $desk = Open-QaDesktop
    $secondLogin = Wait-ForCondition 'second interactive Windows sign-in' 70 {
        if ($desk.HasExited) { throw "Remote Desktop client exited at second sign-in with code $($desk.ExitCode)" }
        Get-QaSession
    }
    if ($secondLogin -eq $firstLogin) { throw 'Windows reused the old session' }
    try {
        $after = Wait-ForCondition 'Task Scheduler to launch Remodex at the second sign-in' 80 {
            $status = Refresh-QaRegistration $productTask
            if ($status.State -eq 'Running' -and $status.LastRun -gt $before.LastRun -and (Get-QaHealth ($port+5))) {
                return $status
            }
        }
    } catch {
        Refresh-QaRegistration $productTask | Format-List
        throw
    }
    $process = @(Get-Process -Name runtime | Where-Object { $_.SessionId -eq $secondLogin -and $_.Path -like "$qa\*" })
    if ($process.Count -ne 2) {
        # The pinned supervisor and its runner both use runtime.exe. Some
        # images expose the prefixed Win32 path only in the CIM process record.
        $process = @(Get-CimInstance Win32_Process -Filter "Name='runtime.exe' AND SessionId=$secondLogin" |
            Where-Object { $_.ExecutablePath -like "*\rmx-sign-in-$env:GITHUB_RUN_ATTEMPT\state\native-installation\*" })
    }
    if ($process.Count -eq 0) { throw 'No pinned runtime was found in the new interactive session' }
    $identity = Get-Content "$qa\state\native-installation\native-install.json" -Raw | ConvertFrom-Json
    if ($identity.environment -ne $initial.environmentId) { throw 'Installation identity changed' }
    $output = [PSCustomObject]@{
        platform = 'Windows Server 2025'
        answeredYes = $true
        firstInteractiveSession = $firstLogin
        signedOutAndServerStopped = $true
        secondInteractiveSession = $secondLogin
        startup = 'Task Scheduler logon trigger'
        newServiceRun = ($after.LastRun -gt $before.LastRun)
        sameInstallation = $true
        healthy = $true
    }
    $output | ConvertTo-Json -Compress
    if ($env:GITHUB_STEP_SUMMARY) {
        "- Windows Server 2025 passed an actual sign-out and second Remote Desktop sign-in (session $firstLogin to $secondLogin). Task Scheduler started the same installation and its health check passed." | Add-Content $env:GITHUB_STEP_SUMMARY
    }
} finally {
    if ($createdUser) {
        Close-QaDesktop
        if ($productTask) { Unregister-ScheduledTask -TaskName $productTask -Confirm:$false -ErrorAction SilentlyContinue }
        Unregister-ScheduledTask -TaskName $bootstrap -Confirm:$false -ErrorAction SilentlyContinue
        Remove-LocalUser -Name $qaUser -ErrorAction SilentlyContinue
    }
}

exit 0
