<#
.SYNOPSIS
Registers or removes the TwitchMiner scheduled task.

.DESCRIPTION
Runs the miner at system startup under the chosen account, whether or not
anyone is logged on, and restarts it after a failure. This is a scheduled task
and not a Windows service: stopping it ends the process without the graceful
shutdown that CTRL-C or SIGTERM gives. See docs/operations.md, "Running without
Docker", before relying on it.

Run from an elevated Windows PowerShell 5.1 or PowerShell 7 session. Sign in
once as the chosen account and run the miner in the foreground first, so the
device login is completed and the session is saved in the data directory.

.PARAMETER ExecutablePath
Full path of twitch-miner.exe.

.PARAMETER DataDirectory
Directory holding config.json and the runtime data. A directory that does not
exist is created and restricted to the chosen account, SYSTEM and Administrators.

.PARAMETER Credential
Account that runs the miner. It needs the "Log on as a batch job" right. It is
prompted for when omitted.

.PARAMETER Uninstall
Stops and removes the task. The data directory is left untouched.
#>
[CmdletBinding(DefaultParameterSetName = 'Install')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Install')]
    [string]$ExecutablePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Install')]
    [string]$DataDirectory,

    [Parameter(Mandatory = $true, ParameterSetName = 'Install')]
    [System.Management.Automation.PSCredential]
    [System.Management.Automation.Credential()]
    $Credential,

    [Parameter(Mandatory = $true, ParameterSetName = 'Uninstall')]
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$taskName = 'TwitchMiner'

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$currentPrincipal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Registering or removing a scheduled task requires an elevated session. Start PowerShell with "Run as administrator".'
}

$existing = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue

if ($Uninstall) {
    if ($null -eq $existing) {
        Write-Output "The $taskName task is not registered."
        return
    }
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
    Write-Output "Removed the $taskName task. The data directory was not touched."
    return
}

if ($null -ne $existing) {
    throw "The $taskName task is already registered. Run this script with -Uninstall first."
}
if (-not [System.IO.Path]::IsPathRooted($ExecutablePath) -or
    -not (Test-Path -LiteralPath $ExecutablePath -PathType Leaf)) {
    throw "ExecutablePath must be the full path of an existing file: $ExecutablePath"
}
if (-not [System.IO.Path]::IsPathRooted($DataDirectory)) {
    throw "DataDirectory must be a full path: $DataDirectory"
}
$ExecutablePath = (Resolve-Path -LiteralPath $ExecutablePath).Path
$account = $Credential.UserName

if (-not (Test-Path -LiteralPath $DataDirectory -PathType Container)) {
    New-Item -ItemType Directory -Path $DataDirectory | Out-Null
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false)
    $inherit = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    foreach ($owner in @($account, 'NT AUTHORITY\SYSTEM', 'BUILTIN\Administrators')) {
        $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
            $owner, 'FullControl', $inherit, 'None', 'Allow')
        $acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $DataDirectory -AclObject $acl
} else {
    Write-Warning "The data directory already exists, so its permissions were left as they are. Check that only $account, SYSTEM and Administrators can read it: it holds the saved Twitch session."
}
$DataDirectory = (Resolve-Path -LiteralPath $DataDirectory).Path

$action = New-ScheduledTaskAction -Execute $ExecutablePath `
    -Argument ('--data-dir "{0}"' -f $DataDirectory) `
    -WorkingDirectory $DataDirectory
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -RestartCount 999 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -MultipleInstances IgnoreNew `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable
$principal = New-ScheduledTaskPrincipal -UserId $account -LogonType Password -RunLevel Limited
$task = New-ScheduledTask -Action $action -Trigger $trigger -Settings $settings -Principal $principal `
    -Description 'Twitch channel-points miner'

Register-ScheduledTask -TaskName $taskName -InputObject $task `
    -User $account -Password $Credential.GetNetworkCredential().Password | Out-Null
Write-Output "Registered the $taskName task for $account. It starts at the next boot; start it now with: Start-ScheduledTask -TaskName $taskName"
