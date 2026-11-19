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
$python = (Get-Command python -CommandType Application).Source

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

    foreach ($index in 1..2) {
        $desk = Open-QaDesktop
        try {
            $session = Wait-ForCondition 'an interactive test desktop' 50 {
                if ($desk.HasExited) { throw "Remote Desktop client exited: $($desk.ExitCode)" }
                Get-QaSession
            }
            Write-Host "Actual Remote Desktop sign-in $index created session $session."
        } catch {
            Get-Content "$qa\desktop.err" -Tail 24 -ErrorAction SilentlyContinue
            Get-Content "$qa\desktop.out" -Tail 24 -ErrorAction SilentlyContinue
            throw
        }
        Close-QaDesktop
        Write-Host "Actual Windows sign-out $index completed."
    }
} finally {
    if ($createdUser) {
        Close-QaDesktop
        Remove-LocalUser -Name $qaUser -ErrorAction SilentlyContinue
    }
}
