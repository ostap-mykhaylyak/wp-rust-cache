# Runs the Rust tests once and the PHP suites on every supported PHP build.
# Windows host with Docker Desktop:  powershell -File tests/matrix.ps1
$root = Split-Path -Parent $PSScriptRoot
$fail = 0
docker run --rm --shm-size=256m -v "${root}:/src" -v wprc-cargo:/usr/local/cargo/registry -v wprc-target-8.4:/src/target wprc-dev:8.4 sh -c "cargo test --workspace -- --test-threads=1 2>&1 | grep -E 'test result|FAILED|panicked'"
if ($LASTEXITCODE -ne 0) { $fail++ }
foreach ($v in @("8.2", "8.3", "8.4", "8.5", "8.4-zts")) {
    docker run --rm --shm-size=256m -v "${root}:/src" -v wprc-cargo:/usr/local/cargo/registry -v "wprc-target-${v}:/src/target" "wprc-dev:$v" sh tests/run-php-tests.sh 2>&1 | Select-String -Pattern "==|checks|identical|FAIL|rror"
    if ($LASTEXITCODE -ne 0) { $fail++; Write-Output "PHP $v FAILED" }
}
Write-Output "matrix: $fail failure(s)"
exit $fail
