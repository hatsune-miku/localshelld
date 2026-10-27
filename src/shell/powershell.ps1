[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding
# PowerShell may prepend its own installation directory on startup. Restore
# the originating user's PATH once, then allow normal session-local mutations.
if (Test-Path Env:LOCALSHELLD_INITIAL_PATH) {
    $env:PATH = $env:LOCALSHELLD_INITIAL_PATH
    Remove-Item Env:LOCALSHELLD_INITIAL_PATH
}
# Command stdin and the protocol reader must never share their read cursor.
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
public sealed class LocalshelldInput : IDisposable {
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr GetStdHandle(int kind);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool SetStdHandle(int kind, IntPtr handle);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool SetHandleInformation(IntPtr handle, uint mask, uint flags);
    readonly IntPtr previous;
    readonly TextReader reader;
    FileStream file;
    public LocalshelldInput(byte[] bytes) {
        previous = GetStdHandle(-10);
        reader = Console.In;
        string path = Path.Combine(Path.GetTempPath(), "localshelld-" + Guid.NewGuid().ToString("N") + ".stdin");
        file = new FileStream(path, FileMode.CreateNew, FileAccess.ReadWrite, FileShare.Read | FileShare.Delete, 4096, FileOptions.DeleteOnClose);
        try {
            file.Write(bytes, 0, bytes.Length);
            file.Flush(); file.Position = 0;
            IntPtr handle = file.SafeFileHandle.DangerousGetHandle();
            if (!SetHandleInformation(handle, 1, 1) || !SetStdHandle(-10, handle))
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            Console.SetIn(new StreamReader(file, new UTF8Encoding(false), true, 4096, true));
        } catch { Dispose(); throw; }
    }
    public void Dispose() {
        SetStdHandle(-10, previous);
        Console.SetIn(reader);
        if (file != null) { file.Dispose(); file = null; }
    }
}
'@
$__localshelld_control = [Console]::In
while (($__localshelld_line = $__localshelld_control.ReadLine()) -ne $null) {
    $__localshelld_parts = $__localshelld_line.Split('|', 3)
    if ($__localshelld_parts.Length -ne 3) { continue }
    $__localshelld_marker = $__localshelld_parts[0]
    $__localshelld_ec = 0
    $__localshelld_input = $null
    try {
        $__localshelld_s = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($__localshelld_parts[1]))
        $__localshelld_input = [LocalshelldInput]::new([Convert]::FromBase64String($__localshelld_parts[2]))
        $global:LASTEXITCODE = 0
        $__localshelld_had_error = $false
        $__localshelld_errors = $global:Error.Count
        . ([ScriptBlock]::Create($__localshelld_s)) 2>&1 | ForEach-Object {
            if ($_ -is [System.Management.Automation.ErrorRecord]) {
                # Native stderr is wrapped as ErrorRecord by a PowerShell
                # pipeline, but its text alone must not change the exit code.
                if ($_.FullyQualifiedErrorId -notin @('NativeCommandError', 'NativeCommandErrorMessage')) {
                    $__localshelld_had_error = $true
                }
                [Console]::Error.Write(($_ | Out-String -Width 32767))
            } else {
                $_ | Out-String -Stream -Width 32767 | ForEach-Object { [Console]::Out.WriteLine($_) }
            }
        }
        $__localshelld_ec = if ($LASTEXITCODE -ne 0) {
            [int]$LASTEXITCODE
        } elseif ($__localshelld_had_error -or $global:Error.Count -gt $__localshelld_errors) {
            1
        } else {
            0
        }
    } catch {
        $__localshelld_ec = 1
        [Console]::Error.Write(($_ | Out-String -Width 32767))
    } finally {
        if ($null -ne $__localshelld_input) { $__localshelld_input.Dispose() }
    }
    $__localshelld_cwd = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((Get-Location).Path))
    $__localshelld_frame = [string][char]30 + $__localshelld_marker + '|' + $__localshelld_ec + '|' + $__localshelld_cwd + [char]31
    [Console]::Out.Write($__localshelld_frame)
    [Console]::Out.Flush()
    [Console]::Error.Write($__localshelld_frame)
    [Console]::Error.Flush()
}
