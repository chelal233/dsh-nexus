param(
    [Parameter(Mandatory=$true)][string]$AgentBinary,
    [Parameter(Mandatory=$true)][string]$LauncherBinary,
    [string]$NodeBinary
)
$ErrorActionPreference = 'Stop'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('nexus-installer-test-' + [Guid]::NewGuid())
$owned = @()
$originalPath = $env:PATH
New-Item -ItemType Directory -Path $fixture > $null
try {
    # Match the installer: run its copied payload from a temporary directory,
    # without any development tools discoverable through PATH.
    $payload = Join-Path $fixture 'nexus-installer-stop.exe'
    Copy-Item -LiteralPath $LauncherBinary -Destination $payload
    $LauncherBinary = $payload
    $env:PATH = ''
    $records = @()
    $unicodeName = 'install ' + [char]0x7528 + [char]0x6237 + ' with spaces & $literal'
    foreach ($name in @($unicodeName, 'other installation')) {
        $install = Join-Path $fixture $name
        $data = Join-Path $fixture ($name + ' data')
        New-Item -ItemType Directory -Path $install > $null
        Copy-Item -LiteralPath $AgentBinary -Destination (Join-Path $install 'nexus-agent.exe')
        $instance = [Guid]::NewGuid().ToString()
        $child = Start-Process -FilePath (Join-Path $install 'nexus-agent.exe') -ArgumentList @('--data-dir', ('"' + $data + '"'), '--port', '0', '--instance-id', $instance) -WindowStyle Hidden -PassThru
        $owned += $child
        $recordPath = Join-Path $data 'run/agent.json'
        $deadline = [DateTime]::UtcNow.AddSeconds(15)
        while (-not (Test-Path -LiteralPath $recordPath)) {
            if ($child.HasExited -or [DateTime]::UtcNow -gt $deadline) { throw 'Fixture Agent failed to start' }
            Start-Sleep -Milliseconds 100
        }
        $records += @{ install = $install; data = $data; recordPath = $recordPath; process = $child }
    }
    $target = $records[0]
    $original = [IO.File]::ReadAllText($target.recordPath)
    $mismatch = $original | ConvertFrom-Json
    $mismatch.instance_id = 'wrong-instance'
    [IO.File]::WriteAllText($target.recordPath, ($mismatch | ConvertTo-Json))
    & $LauncherBinary installer-stop --install-dir $target.install
    if ($LASTEXITCODE -eq 0 -or $target.process.HasExited) { throw 'Mismatched discovery must fail without stopping Agent' }
    [IO.File]::WriteAllText($target.recordPath, $original)
    & $LauncherBinary installer-stop --install-dir $target.install
    if ($LASTEXITCODE -ne 0 -or -not $target.process.HasExited) { throw 'Successful installer shutdown must wait for actual Agent exit' }
    if ($records[1].process.HasExited) { throw 'Another installation was stopped' }
    if (-not (Test-Path -LiteralPath $target.data)) { throw 'Data root was removed' }
    & $LauncherBinary installer-stop --install-dir $target.install
    if ($LASTEXITCODE -ne 0) { throw 'Stopped Agent with stale discovery must be idempotent' }
    & $LauncherBinary installer-stop --install-dir (Join-Path $fixture 'fresh install')
    if ($LASTEXITCODE -ne 0) { throw 'Fresh install should not require an existing directory' }
    Write-Output 'PASS: mismatched identity rejected; actual process exit; other installation preserved; data preserved; repeated stop; fresh install.'
    if ($NodeBinary) {
        # A fixture with valid identity that accepts shutdown but never exits.
        # It proves the installer cannot mistake HTTP 202 for a stopped process.
        $stubborn = Join-Path $fixture 'stubborn installation'
        $stubbornData = Join-Path $fixture 'stubborn data'
        New-Item -ItemType Directory -Path $stubborn > $null
        Copy-Item -LiteralPath $NodeBinary -Destination (Join-Path $stubborn 'nexus-agent.exe')
        $server = @'
const fs = require('fs');
const path = require('path');
const http = require('http');
const data = process.argv[process.argv.indexOf('--data-dir') + 1];
const instance = process.argv[process.argv.indexOf('--instance-id') + 1];
const server = http.createServer((req, res) => {
  res.setHeader('Content-Type', 'application/json');
  if (req.url === '/v1/health') res.end(JSON.stringify({api_version:'v1',service:'nexus-agent',instance_id:instance,data_root_id:'fixture'}));
  else { res.statusCode = 202; res.end('{}'); }
});
server.listen(0, '127.0.0.1', () => {
  fs.mkdirSync(path.join(data,'run'), {recursive:true});
  fs.writeFileSync(path.join(data,'run','agent.json'), JSON.stringify({pid:process.pid,port:server.address().port,instance_id:instance,data_root_id:'fixture'}));
});
'@
        $script = Join-Path $stubborn 'fixture.cjs'
        [IO.File]::WriteAllText($script, $server)
        $child = Start-Process -FilePath (Join-Path $stubborn 'nexus-agent.exe') -ArgumentList @(('"' + $script + '"'), '--data-dir', ('"' + $stubbornData + '"'), '--instance-id', 'stubborn') -WindowStyle Hidden -PassThru
        $owned += $child
        $deadline = [DateTime]::UtcNow.AddSeconds(15)
        while (-not (Test-Path -LiteralPath (Join-Path $stubbornData 'run/agent.json'))) {
            if ($child.HasExited -or [DateTime]::UtcNow -gt $deadline) { throw 'Stubborn fixture failed to start' }
            Start-Sleep -Milliseconds 100
        }
        $watch = [Diagnostics.Stopwatch]::StartNew()
        & $LauncherBinary installer-stop --install-dir $stubborn
        if ($LASTEXITCODE -eq 0 -or $child.HasExited -or $watch.Elapsed.TotalSeconds -lt 30 -or $watch.Elapsed.TotalSeconds -gt 65) { throw 'Shutdown acceptance must time out without killing the Agent or allowing replacement' }
        Write-Output 'PASS: HTTP shutdown acceptance without process exit times out and preserves the running process.'
    }
} finally {
    $env:PATH = $originalPath
    foreach ($child in $owned) {
        if (-not $child.HasExited) { $child.Kill(); $child.WaitForExit() }
        $child.Dispose()
    }
    # Only this test's newly-created random fixture is removed.
    if ([IO.Path]::GetFullPath($fixture).StartsWith([IO.Path]::GetFullPath([IO.Path]::GetTempPath()), [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $fixture -Recurse -Force
    }
}
