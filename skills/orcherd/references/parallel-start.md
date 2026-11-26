# Starting independent seats in parallel

Each `orcherd spawn` that starts a cold terminal waits for `herdr-core` and the agent harness to become ready (typically 5–10 seconds). Because agent tool calls often execute sequentially, separate tool calls do not overlap. Start a batch of up to 4 independent seats in a single shell command instead, then **end your turn**.

## Rules

1. Write each seat's brief to its own file (`briefs/w1.md`, `briefs/w2.md`) first and pass `--spec-file`, avoiding shell quoting pitfalls.
2. Give each `spawn` its own stdout receipt file and stderr file, and collect every exit code.
3. Read every JSON receipt after `wait`. If a receipt reports `failed_stage` or `next`, follow it rather than blindly re-running `spawn`.
4. After the batch starts, **end your turn**—`orcherd daemon` wakes you as each worker reports `done`, `ask`, or `escalation`.

## POSIX shells (`bash`, `zsh`)

Substitute your resolved `ORCHERD` executable path:

```sh
d=$(mktemp -d)
ORCHERD spawn --seat zcode1 --agent zcode --worktree new-child --spec-file briefs/t1.md --json >"$d/zcode1.out" 2>"$d/zcode1.err" & p1=$!
ORCHERD spawn --seat zcode2 --agent zcode --worktree new-child --spec-file briefs/t2.md --json >"$d/zcode2.out" 2>"$d/zcode2.err" & p2=$!
s=0; wait "$p1" || s=$?; echo "zcode1 exit $s"
s=0; wait "$p2" || s=$?; echo "zcode2 exit $s"
for f in "$d"/*; do echo "== $f"; cat "$f"; done
```

## Windows PowerShell (5.1+)

```powershell
$d = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ([guid]::NewGuid()))
$starts = foreach ($n in 'zcode1', 'zcode2') {
  $p = Start-Process -FilePath 'ORCHERD' -NoNewWindow -PassThru -WorkingDirectory (Get-Location) `
    -ArgumentList "spawn --seat $n --agent zcode --worktree new-child --spec-file briefs\$n.md --json" `
    -RedirectStandardOutput "$d\$n.out" -RedirectStandardError "$d\$n.err"
  $null = $p.Handle
  [pscustomobject]@{ Name = $n; Process = $p }
}
foreach ($s in $starts) { $s.Process.WaitForExit(); "$($s.Name) exit $($s.Process.ExitCode)" }
Get-ChildItem $d | ForEach-Object { "== $($_.Name)"; Get-Content $_.FullName }
```
