# Owned resource cleanup

CLI tasks persist a repository-local manifest under `.repo-sandbox/tasks` with
task/repository identity, exact resource identifiers and expected owner markers.
Normal runs clean their one-shot container; `clean` handles retained resources.

`repo-sandbox clean --repository PATH --dry-run` uses the same plan as real
cleanup and changes nothing. Images and local cache require explicit
`--include-images`/`--include-cache`. Automation supplies `--yes`. Container and
image cleanup requires both the exact task/image identity label and repository
digest label. Filesystem owner markers are checked again; a forged or
cross-repository manifest is refused. One failure does not stop later candidates
and makes the final command non-zero. Successful removal appends an atomic
`cleaned` journal event, making repeated cleanup idempotent.

Reports and exported artifacts are never candidates. No path calls Docker
system/buildx prune, deletes shared cache, or deletes Registry content.
