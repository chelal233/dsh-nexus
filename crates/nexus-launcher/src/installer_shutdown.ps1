$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

try {
    $directory = [IO.Path]::GetFullPath($env:NEXUS_INSTALL_STOP_DIRECTORY)
    $agentPath = [IO.Path]::Combine($directory, 'nexus-agent.exe')
    # Fresh installation needs no process/data-root discovery.
    if (-not [IO.File]::Exists($agentPath)) { exit 0 }

    Add-Type -AssemblyName System.Net.Http
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class NexusInstallerArguments {
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr CommandLineToArgvW(string command, out int count);
    [DllImport("kernel32.dll")]
    static extern IntPtr LocalFree(IntPtr memory);
    public static string[] Parse(string command) {
        int count;
        IntPtr memory = CommandLineToArgvW(command, out count);
        if (memory == IntPtr.Zero) throw new InvalidOperationException("Cannot read Agent arguments");
        try {
            string[] values = new string[count];
            for (int i = 0; i < count; i++) values[i] = Marshal.PtrToStringUni(Marshal.ReadIntPtr(memory, i * IntPtr.Size));
            return values;
        } finally { LocalFree(memory); }
    }
}
'@
    $handler = New-Object System.Net.Http.HttpClientHandler
    $handler.UseProxy = $false
    $handler.AllowAutoRedirect = $false
    $client = New-Object System.Net.Http.HttpClient($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(5)
    try {
        $candidates = @(Get-CimInstance Win32_Process -Filter "Name = 'nexus-agent.exe'")
        foreach ($candidate in $candidates) {
            if (-not $candidate.ExecutablePath) { throw 'Cannot verify the installation path of a running Agent. Close it and retry.' }
            if (-not [StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($candidate.ExecutablePath), $agentPath)) { continue }
            # Hold an actual process handle, not a PID-only wait that can race reuse.
            try { $agentProcess = [Diagnostics.Process]::GetProcessById([int]$candidate.ProcessId) }
            catch [ArgumentException] { continue }
            try {
                $null = $agentProcess.Handle
                if ($agentProcess.HasExited) { continue }
                if (-not [StringComparer]::OrdinalIgnoreCase.Equals($agentProcess.MainModule.FileName, $agentPath)) { throw 'Agent process identity changed. Retry installation.' }
                $arguments = [NexusInstallerArguments]::Parse($candidate.CommandLine)
                $rootIndex = [Array]::IndexOf($arguments, '--data-dir')
                $instanceIndex = [Array]::IndexOf($arguments, '--instance-id')
                if ($rootIndex -lt 0 -or $rootIndex + 1 -ge $arguments.Length -or $instanceIndex -lt 0 -or $instanceIndex + 1 -ge $arguments.Length) { throw 'Agent discovery arguments are unavailable. Stop this Agent manually and retry.' }
                $dataRoot = $arguments[$rootIndex + 1]
                if (-not [IO.Path]::IsPathRooted($dataRoot)) { throw 'Agent data root is not absolute.' }
                $recordPath = [IO.Path]::Combine($dataRoot, 'run', 'agent.json')
                $record = Get-Content -LiteralPath $recordPath -Raw | ConvertFrom-Json
                if ($record.pid -ne $candidate.ProcessId -or $record.instance_id -ne $arguments[$instanceIndex + 1] -or $record.port -lt 1 -or $record.port -gt 65535) { throw 'Agent discovery identity does not match the installed process.' }
                $base = 'http://127.0.0.1:' + $record.port
                $healthResponse = $client.GetAsync($base + '/v1/health').GetAwaiter().GetResult()
                try {
                    $null = $healthResponse.EnsureSuccessStatusCode()
                    $health = $healthResponse.Content.ReadAsStringAsync().GetAwaiter().GetResult() | ConvertFrom-Json
                } finally { $healthResponse.Dispose() }
                if ($health.service -ne 'nexus-agent' -or $health.api_version -ne 'v1' -or $health.instance_id -ne $record.instance_id -or $health.data_root_id -ne $record.data_root_id) { throw 'Agent health identity does not match discovery.' }
                $request = New-Object System.Net.Http.HttpRequestMessage([System.Net.Http.HttpMethod]::Post, ($base + '/v1/shutdown'))
                try {
                    $request.Headers.Add('x-nexus-data-root-id', [string]$record.data_root_id)
                    $request.Headers.Add('x-nexus-instance-id', [string]$record.instance_id)
                    $response = $client.SendAsync($request).GetAwaiter().GetResult()
                    try { $null = $response.EnsureSuccessStatusCode() } finally { $response.Dispose() }
                } finally { $request.Dispose() }
                if (-not $agentProcess.WaitForExit(30000)) { throw 'Agent is still stopping after 30 seconds. Retry when shutdown completes.' }
            } finally { $agentProcess.Dispose() }
        }
    } finally { $client.Dispose(); $handler.Dispose() }
    # A legacy supervisor may have relaunched Agent while its predecessor exited.
    foreach ($remaining in @(Get-CimInstance Win32_Process -Filter "Name = 'nexus-agent.exe'")) {
        if (-not $remaining.ExecutablePath) { throw 'Cannot verify a remaining Agent process.' }
        if ([StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($remaining.ExecutablePath), $agentPath)) { throw 'Agent restarted during shutdown. Close its launcher and retry installation.' }
    }
    exit 0
} catch {
    [Console]::Error.WriteLine('Nexus installation stopped before replacing files: ' + $_.Exception.Message)
    exit 1
}
