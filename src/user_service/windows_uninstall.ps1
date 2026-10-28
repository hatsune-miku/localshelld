# Task Scheduler operations stay in COM: localized schtasks output is not UTF-8.
function Get-UserTaskOrNull($folder, [string]$name) {
    try { return $folder.GetTask($name) } catch {
        $exception = $_.Exception
        while ($null -ne $exception) {
            if ($exception.HResult -eq -2147024894) { return $null } # ERROR_FILE_NOT_FOUND
            $exception = $exception.InnerException
        }
        throw
    }
}

function Remove-LocalshelldUserTask($folder, [string]$name, [string]$sid, [int]$timeoutMilliseconds = 30000) {
    $task = Get-UserTaskOrNull $folder $name
    if ($null -eq $task) { return 'missing' }

    # Never manage another user's task just because its name resembles ours.
    $definition = $task.Definition
    $owner = $definition.Principal.UserId
    if (-not $owner.StartsWith('S-1-')) {
        $owner = ([Security.Principal.NTAccount]$owner).Translate([Security.Principal.SecurityIdentifier]).Value
    }
    if ($owner -ne $sid -or $definition.Principal.RunLevel -ne 0 -or $definition.Principal.LogonType -ne 3) {
        throw 'User task has a different identity or privilege level; registration was preserved.'
    }
    if ($definition.Actions.Count -ne 1 -or $definition.Actions.Item(1).Type -ne 0 -or
        $definition.Actions.Item(1).Arguments -cnotmatch '^(?:serve --user )?--config "[^"\r\n]+"$') {
        throw 'User task has a different action; registration was preserved.'
    }

    # Disabling first prevents a login/restart trigger racing the removal, and
    # surfaces a read-only task ACL before pretending an uninstall succeeded.
    $task.Enabled = $false
    if ($task.State -eq 4 -or $task.GetInstances(0).Count -gt 0) {
        $task.Stop(0)
    }
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($task.State -in @(2, 4) -or $task.GetInstances(0).Count -gt 0) {
        if ($timer.ElapsedMilliseconds -ge $timeoutMilliseconds) {
            throw 'User task did not stop before the timeout; it remains disabled and registered.'
        }
        Start-Sleep -Milliseconds 100
    }
    $folder.DeleteTask($name, 0)
    if ($null -ne (Get-UserTaskOrNull $folder $name)) {
        throw 'User task is still registered after deletion; uninstall is incomplete.'
    }
    return 'removed'
}
