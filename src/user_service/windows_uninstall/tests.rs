use super::*;

const MOCKS: &str = r#"
function New-FakeFolder([string]$mode = '') {
    $action = [pscustomobject]@{Type=0; Arguments='serve --user --config "C:\test.toml"'}
    $actions = [pscustomobject]@{Count=1; Action=$action}
    $actions | Add-Member ScriptMethod Item { param($index) return $this.Action }
    $task = [pscustomobject]@{
        Definition=[pscustomobject]@{Principal=[pscustomobject]@{UserId='S-1-5-21-123'; RunLevel=0; LogonType=3}; Actions=$actions}
        EnableValue=$true; Running=$true; Stopped=$false; Mode=$mode
    }
    $task | Add-Member ScriptProperty Enabled { $this.EnableValue } {
        param($value)
        if ($this.Mode -eq 'disable-denied') { throw [Runtime.InteropServices.COMException]::new('拒绝访问', -2147024891) }
        $this.EnableValue=$value
    }
    $task | Add-Member ScriptProperty State { if ($this.Running) { 4 } elseif ($this.Enabled) { 3 } else { 1 } }
    $task | Add-Member ScriptMethod GetInstances { param($flags) return [pscustomobject]@{Count=[int]$this.Running} }
    $task | Add-Member ScriptMethod Stop {
        param($flags)
        if ($this.Mode -eq 'stop-denied') { throw [Runtime.InteropServices.COMException]::new('拒绝停止', -2147024891) }
        if ($this.Mode -ne 'never-stops') { $this.Running=$false }
        $this.Stopped=$true
    }
    $folder = [pscustomobject]@{Task=$task; Deleted=$false; Mode=$mode}
    $folder | Add-Member ScriptMethod GetTask {
        param($name)
        if ($name -ne 'fixture') { throw 'unexpected task name' }
        if ($this.Mode -eq 'query-denied') { throw [Runtime.InteropServices.COMException]::new('拒绝读取', -2147024891) }
        if ($null -eq $this.Task) { throw [Runtime.InteropServices.COMException]::new('missing', -2147024894) }
        return $this.Task
    }
    $folder | Add-Member ScriptMethod DeleteTask {
        param($name, $flags)
        if ($name -ne 'fixture') { throw 'unexpected deletion target' }
        if ($this.Mode -eq 'delete-denied') { throw [Runtime.InteropServices.COMException]::new('拒绝删除', -2147024891) }
        if ($this.Mode -ne 'sticky-delete') { $this.Task=$null; $this.Deleted=$true }
    }
    return $folder
}
"#;

fn mock_script(body: &str) -> Result<String> {
    run_script(&format!("{MOCKS}\n{body}"))
}

#[test]
fn removes_running_or_stopped_tasks_and_is_idempotent() {
    mock_script(
        r#"
foreach ($running in @($true, $false)) {
    $folder = New-FakeFolder
    $task = $folder.Task
    $task.Running = $running
    if ((Remove-LocalshelldUserTask $folder 'fixture' 'S-1-5-21-123') -ne 'removed') { throw 'not removed' }
    if (-not $folder.Deleted -or $task.Enabled -or $task.Running -or $task.Stopped -ne $running) { throw 'incorrect lifecycle' }
    if ((Remove-LocalshelldUserTask $folder 'fixture' 'S-1-5-21-123') -ne 'missing') { throw 'repeat removal failed' }
}
"#,
    )
    .unwrap();
}

#[test]
fn refuses_changed_identity_privileges_or_action_before_mutating_a_task() {
    mock_script(
        r#"
foreach ($mutation in @(
    {$task.Definition.Principal.UserId='S-1-5-18'},
    {$task.Definition.Principal.RunLevel=1},
    {$task.Definition.Principal.LogonType=5},
    {$task.Definition.Actions.Count=2},
    {$task.Definition.Actions.Action.Type=5},
    {$task.Definition.Actions.Action.Arguments='serve --config "C:\test.toml"'}
)) {
    $folder = New-FakeFolder
    $task = $folder.Task
    & $mutation
    $rejected = $false
    try { Remove-LocalshelldUserTask $folder 'fixture' 'S-1-5-21-123' } catch { $rejected=$true }
    if (-not $rejected -or -not $task.Enabled -or $task.Stopped -or $folder.Deleted) { throw 'foreign task was changed' }
}
"#,
    )
    .unwrap();
}

#[test]
fn never_reports_success_after_permission_stop_timeout_or_delete_failures() {
    mock_script(
        r#"
foreach ($mode in @('disable-denied', 'stop-denied', 'delete-denied', 'query-denied', 'never-stops', 'sticky-delete')) {
    $folder = New-FakeFolder $mode
    $failed = $false
    try { Remove-LocalshelldUserTask $folder 'fixture' 'S-1-5-21-123' 0 } catch { $failed=$true }
    if (-not $failed -or $folder.Deleted -or $null -eq $folder.Task) { throw "failure was ignored: $mode" }
}
"#,
    )
    .unwrap();
}

#[test]
fn access_denied_diagnostics_are_unicode_and_explain_same_user_elevation() {
    let error = run_script(
        "throw [Runtime.InteropServices.COMException]::new('拒绝访问：测试任务', -2147024891)",
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("拒绝访问：测试任务"), "{message}");
    assert!(message.contains("0x80070005"), "{message}");
    assert!(message.contains("SAME logged-in user"), "{message}");
    assert!(!message.contains('\u{fffd}'), "{message}");
}

#[test]
fn removes_only_a_disposable_disabled_registration_via_real_task_scheduler() {
    let directory = tempfile::tempdir().unwrap();
    let saved: [(&str, &[u8]); 3] = [
        ("fixture.toml", b"existing configuration"),
        ("seal.key", b"existing seal key"),
        (
            "fixture.authorization.json",
            b"existing pairing and passkeys",
        ),
    ];
    for (name, bytes) in saved {
        std::fs::write(directory.path().join(name), bytes).unwrap();
    }
    let sid = super::super::sid().unwrap();
    let name = format!("localshelld-uninstall-test-{}", uuid::Uuid::new_v4());
    let document = super::super::scheduled_task(
        &std::env::current_exe().unwrap(),
        &directory.path().join("fixture.toml"),
        &sid,
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(document.as_bytes());
    // No logon trigger and no task execution: this fixture never starts a daemon
    // or grants access. TASK_CREATE refuses to overwrite an existing task.
    let output = run_script(&format!(
        r#"
$service = New-Object -ComObject Schedule.Service
$service.Connect()
$folder = $service.GetFolder('\')
$definition = $service.NewTask(0)
$definition.XmlText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}'))
$definition.Triggers.Clear()
$definition.Settings.Enabled = $false
$created = $false
try {{
    $null = $folder.RegisterTaskDefinition('{name}', $definition, 2, '{sid}', $null, 3)
    $created = $true
    if ((Remove-LocalshelldUserTask $folder '{name}' '{sid}') -ne 'removed') {{ throw 'real task removal failed' }}
    if ((Remove-LocalshelldUserTask $folder '{name}' '{sid}') -ne 'missing') {{ throw 'real repeated removal failed' }}
    'disposable task removed'
}} finally {{
    if ($created -and $null -ne (Get-UserTaskOrNull $folder '{name}')) {{ $folder.DeleteTask('{name}', 0) }}
}}
"#
    ))
    .unwrap();
    assert!(output.contains("disposable task removed"), "{output}");
    for (name, bytes) in saved {
        assert_eq!(std::fs::read(directory.path().join(name)).unwrap(), bytes);
    }
}
