# Isolated task batches

Prepare up to three dependency-ready tasks in separate Git worktrees, then explicitly start one bounded attempt per lane. Use this for tasks whose implementation areas have been reviewed for overlap. Task independence is a design judgment; dependency readiness alone does not prove it.

This Linux coordinator uses Python 3, Git, SQLite, `flock` support and GNU `timeout`. Workers use the phase's configured OpenCode model. No additional provider credential is stored in the manifest. The coordinator retains existing build/test gates and never invokes inference during preparation.

## Preparation and execution

Build the fork (`cargo build -p dial-cli --locked`), then use the resulting absolute binary path. All other sequential launchers for the source must also use a batch-capable binary, so the persistent reservation guard is enforced. Existing running workers must finish and be reviewed first. Do not replace an executable in place while it is running; install by an atomic rename or use a separate path.

The plan declares one task and distinct live-view port per lane plus all locks used by existing source launchers. The included Bible Study plan assigns tasks 10, 19 and 21. Tasks 11 and 12 follow task 10 in later reviewed batches; they do not run concurrently with their prerequisites.

```bash
python3 tools/dial_batch.py prepare \
  --source /absolute/path/to/project \
  --plan examples/bible-study-batch.json \
  --destination /absolute/path/to/new-batch-directory \
  --binary /absolute/path/to/batch-capable-dial
```

Preparation refuses dirty/active source work, stale signals, incomplete dependencies, duplicate assignments, occupied ports, existing destinations and existing reservations. It snapshots SQLite with the backup API, preserving the source queue and failure history. Each local queue is a full snapshot with only the execution pin and live-view port changed. Unassigned tasks remain unchanged, rather than being falsely completed or cancelled. The tracked root `UserDB.sqlite` is excluded from the worktree checkout; each worker receives a fresh, isolated mutable database path. The source's personal database is never read by the coordinator.

Prepare dependencies in each lane using the project's established setup commands, such as `npm ci --prefix modules/core/frontend`. Verify required immutable resources before starting; do not copy live databases or rebuild large resources incidentally. Each lane has a separate build target and frontend output. The Bible Study smoke harness already allocates its API/browser ports dynamically. Other projects must provide similarly isolated test ports and paths; different live-view ports alone are insufficient.

Preparation does not start workers. After separate authorization:

```bash
python3 tools/dial_batch.py start /absolute/path/to/new-batch-directory/batch.json
python3 tools/dial_batch.py status /absolute/path/to/new-batch-directory/batch.json
```

Start rechecks the source revision/queue/configuration and all lane preconditions before starting any worker. It holds the source launcher locks until all attempts return. Each lane runs `auto-run --cli opencode --max 1 --max-attempts 1`, with a 45-minute outer bound, no automatic retry and an 8 MiB coordinator log cap. SIGINT/SIGTERM requests termination only of owned child process groups. Existing Dial worker logs remain governed by Dial's own retention. Each lane's `.dial/batch-attempt.log`, queue and Git branch retain evidence for review.

An exit code of zero means **awaiting review**, not acceptance. No source queue completion, merge, push, dependency release, resource regeneration or next-task launch occurs automatically. Failed preparation retains its reservation and partial artifacts; inspect them rather than automatically deleting them. An interrupted coordinator remains fail-closed; inspect owned processes and iteration records before recovery. Never erase failed iterations to make a batch reusable; prepare a fresh batch after review.

## Integration checkpoint

1. Inspect each lane's task/iteration status, actual verification evidence and full diff from the recorded base. Confirm it modified only its approved scope; a pin controls scheduling, not filesystem access.
2. Integrate accepted commits serially into a clean integration checkout, resolving shared tests/spec append conflicts deliberately. Do not run workers in that checkout. Git worktrees share refs/stashes; never stash/reset other work or let a worker manage other worktrees.
3. Run the project's required combined verification on the integrated revision. Keep native/operator observations separate from browser/API checks. Record evidence in tracked canonical specs/tests.
4. Reconcile accepted task outcomes into the source queue under its launcher locks, preserving lifetime attempts/failures and historical iterations. This is a supervised operation, not a blind copy of a worker database. Do not mark a task complete merely because its branch exists.
5. After explicitly reviewing all lanes, release the reservation:

```bash
python3 tools/dial_batch.py release /absolute/path/to/new-batch-directory/batch.json --reviewed
```

Release only removes the scheduling reservation; it neither accepts results nor deletes worktrees/logs. A source task not accepted remains pending/blocked. Choose the next batch from the newly integrated base so dependency results are actually present. Retain only the needed batch worktrees/build caches after handoff; cleanup is a separate reviewed operation.

## Selection contract

`execution_task_id` is an optional positive task ID in a lane's configuration. All next-task selection paths (task next, iterate, auto-run, context, orchestration and dry-run) use one selector. It honors dependencies, errors on malformed/nonexistent pins, and never falls back to another task after the pinned task completes or becomes unavailable. `.dial/batch-reservation.json` refuses new task selection in the source checkout. These are orchestration guards, not a security sandbox against a worker explicitly editing configuration or another directory.

## Verification

```bash
cargo fmt --all -- --check
cargo test -p dial-cli --test auto_run_limits --locked -- --test-threads=1
python3 tools/test_dial_batch.py -v
cargo test -p dial-core --test engine_tests --locked -- --test-threads=1
```

The Python suite defaults to `target/debug/dial`; set `DIAL_TEST_BINARY` for another built binary. Tests use synthetic repositories, test ports and fake workers, never real inference. They cover concurrent task identity, queue/history preservation, personal-database exclusion, dependencies, reservations, dirty/active work, occupied ports, changed source state, and failure/no-retry behavior.

## Preparation evidence (2026-11-06)

The initial implementation passed 16 auto-run integration tests, six Python batch tests (including three concurrent fake workers), and 75 engine integration tests. `cargo fmt --all -- --check` and `git diff --check` passed. The sandbox initially denied local socket creation and process signaling; the affected fake-worker suites passed outside that sandbox. No real model/provider throughput, application batch, integrated application build or native checkpoint has been exercised by these tooling tests.

For Bible Study, the existing Task 08 worker was active during preparation, so the application checkout, canonical queue and personal data were left untouched. The example amendment is ready to append to `specs/buglist/spec.md` under both source launcher locks once active work is reviewed. The example plan is a proposed first batch, not a reservation or execution record. The optional direct supervisor task has not been assigned or started.
