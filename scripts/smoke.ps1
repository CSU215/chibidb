# End-to-end smoke test: server -> client SQL -> crash -> WAL recovery.
# Usage: powershell -File scripts\smoke.ps1   (from the repo root)

# NOTE: no $ErrorActionPreference="Stop" here: cargo writes to stderr and
# PowerShell 5.1 turns that into a terminating error when Stop is active.
$dir = "tmp\smoke"

if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
New-Item -ItemType Directory -Force $dir | Out-Null

cargo build -q
if ($LASTEXITCODE -ne 0) { throw "build failed" }

# start the server in the background
$job = Start-Job -ScriptBlock {
    Set-Location $using:PWD
    cargo run -q -- serve $using:dir 2>$null
}
Start-Sleep -Seconds 4

# exercise SQL through the wire protocol
$sql = @"
create table t (id int, name char(10));
insert into t values (1, 'alice'), (2, 'bob');
create index idx on t (id);
explain select * from t where id = 2;
select * from t where id = 2;
begin;
insert into t values (3, 'carol');
commit;
exit
"@
$out = $sql | cargo run -q -- client 2>$null
$out | ForEach-Object { $_ }

# kill the server WITHOUT a clean flush: only the WAL is durable
Stop-Job $job -ErrorAction SilentlyContinue
Remove-Job $job -Force -ErrorAction SilentlyContinue

# reopen the directory: committed data (including the index path) must survive
$sql = @"
select * from t order by id;
exit
"@
$after = $sql | cargo run -q -- $dir 2>$null
$after | ForEach-Object { $_ }

if (($after -join "`n") -match "alice" -and ($after -join "`n") -match "carol") {
    Write-Host "`nSMOKE OK: committed data survived the crash via WAL" -ForegroundColor Green
    exit 0
} else {
    Write-Host "`nSMOKE FAILED: committed data lost" -ForegroundColor Red
    exit 1
}
