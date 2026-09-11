$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$script:InstallerClock = [Diagnostics.Stopwatch]::StartNew()
$script:InstallerBudgetMs = if ($env:NEXUS_INSTALL_STOP_BUDGET_MS) { [int]$env:NEXUS_INSTALL_STOP_BUDGET_MS } else { 150000 }
function Remaining-InstallerMilliseconds([int]$Maximum) {
    $remaining = $script:InstallerBudgetMs - $script:InstallerClock.ElapsedMilliseconds
    if ($remaining -le 0) { throw 'Installer shutdown reached its overall deadline. Retry when Agent shutdown completes.' }
    return [int][Math]::Min($Maximum, $remaining)
}

function Invoke-InstallerHelper([string]$Argument, [string]$Executable = $env:NEXUS_INSTALL_STOP_HELPER, [int]$TimeoutMs = 45000) {
    # MSI extracts a PE with a .tmp suffix. Execute it directly, without shell
    # file associations or PowerShell's extension-based command discovery.
    $process = New-Object Diagnostics.Process
    $process.StartInfo.FileName = $Executable
    $process.StartInfo.Arguments = $Argument
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    $process.StartInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $process.StartInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    try {
        $budget = Remaining-InstallerMilliseconds $TimeoutMs
        [void]$process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($budget)) {
            $process.Kill() # Only this disposable helper, never an Agent.
            [void]$process.WaitForExit(5000)
            throw 'Installer helper exceeded its execution deadline.'
        }
        $output = $stdout.GetAwaiter().GetResult()
        $errorOutput = $stderr.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) {
            throw ('Installer helper exited with code ' + $process.ExitCode + ': ' + $errorOutput)
        }
        return $output
    } finally { $process.Dispose() }
}

function Test-HarnessCommandOwner([string[]]$Arguments) {
    # This exact internal entry point is a child of the real Agent's Job.
    # Defer it until its Agent shuts down; the final process scan still rejects
    # every surviving process at this installation path, including owners.
    return ($Arguments.Length -ge 3 -and $Arguments[1] -ceq '--harness-command')
}

try {
    $directory = [IO.Path]::GetFullPath($env:NEXUS_INSTALL_STOP_DIRECTORY)
    $agentPath = [IO.Path]::Combine($directory, 'nexus-agent.exe')
    # Fresh installation needs no process/data-root discovery.
    if (-not [IO.File]::Exists($agentPath)) { exit 0 }

    # Load a shipped system assembly; this does not compile source code.
    [void][Reflection.Assembly]::Load('System.Net.Http, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b03f5f7f11d50a3a')
    [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
    $handler = New-Object System.Net.Http.HttpClientHandler
    $handler.UseProxy = $false
    $handler.AllowAutoRedirect = $false
    $client = New-Object System.Net.Http.HttpClient($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(5)
    try {
        $candidates = @(Get-CimInstance Win32_Process -Filter "Name = 'nexus-agent.exe'")
        foreach ($candidate in $candidates) {
            $null = Remaining-InstallerMilliseconds 1
            if (-not $candidate.ExecutablePath) { throw 'Cannot verify the installation path of a running Agent. Close it and retry.' }
            if (-not [StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($candidate.ExecutablePath), $agentPath)) { continue }
            # Hold an actual process handle, not a PID-only wait that can race reuse.
            try { $agentProcess = [Diagnostics.Process]::GetProcessById([int]$candidate.ProcessId) }
            catch [ArgumentException] { continue }
            try {
                $null = $agentProcess.Handle
                if ($agentProcess.HasExited) { continue }
                if (-not [StringComparer]::OrdinalIgnoreCase.Equals($agentProcess.MainModule.FileName, $agentPath)) { throw 'Agent process identity changed. Retry installation.' }
                # The command line is data, never interpolated into shell code.
                $env:NEXUS_INSTALL_STOP_COMMAND_LINE = $candidate.CommandLine
                try {
                    $argumentJson = Invoke-InstallerHelper 'installer-parse-arguments'
                    $arguments = $argumentJson | ConvertFrom-Json
                } finally { Remove-Item Env:NEXUS_INSTALL_STOP_COMMAND_LINE -ErrorAction SilentlyContinue }
                if (Test-HarnessCommandOwner $arguments) { continue }
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
                $credentialPath = [IO.Path]::Combine($dataRoot, 'run', 'agent-credential.json')
                # Read protocol support from the verified installed executable, not untrusted HTTP health.
                $binaryIdentity = Invoke-InstallerHelper '--build-identity' $agentPath 10000 | ConvertFrom-Json
                $supportsAuth = $binaryIdentity.PSObject.Properties.Name -contains 'authVersion'
                if ($supportsAuth -or [IO.File]::Exists($credentialPath)) {
                    $env:NEXUS_INSTALL_AUTH_ROOT = $dataRoot
                    $env:NEXUS_INSTALL_AUTH_PID = [string]$candidate.ProcessId
                    $env:NEXUS_INSTALL_AUTH_INSTANCE = [string]$record.instance_id
                    try {
                        $null = Invoke-InstallerHelper 'installer-authenticated-shutdown'
                    } finally {
                        Remove-Item Env:NEXUS_INSTALL_AUTH_ROOT, Env:NEXUS_INSTALL_AUTH_PID, Env:NEXUS_INSTALL_AUTH_INSTANCE -ErrorAction SilentlyContinue
                    }
                } else {
                    if ($binaryIdentity.version -notin @('0.1.0', '0.1.1') -or -not $binaryIdentity.buildId -or $binaryIdentity.buildId -eq 'development') { throw 'Unknown legacy Agent build; stop it manually before installation.' }
                    $listeners = @(Get-NetTCPConnection -LocalPort $record.port -State Listen -ErrorAction Stop | Where-Object { $_.LocalAddress -eq '127.0.0.1' })
                    if ($agentProcess.HasExited -or $listeners.Count -ne 1 -or $listeners[0].OwningProcess -ne $candidate.ProcessId) { throw 'Legacy Agent listener does not belong to the verified process.' }
                    $request = New-Object System.Net.Http.HttpRequestMessage([System.Net.Http.HttpMethod]::Post, ($base + '/v1/shutdown'))
                    try {
                        $request.Headers.Add('x-nexus-data-root-id', [string]$record.data_root_id)
                        $request.Headers.Add('x-nexus-instance-id', [string]$record.instance_id)
                        $response = $client.SendAsync($request).GetAwaiter().GetResult()
                        try { $null = $response.EnsureSuccessStatusCode() } finally { $response.Dispose() }
                    } finally { $request.Dispose() }
                }
                if (-not $agentProcess.WaitForExit((Remaining-InstallerMilliseconds 30000))) { throw 'Agent is still stopping. Retry when shutdown completes.' }
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
