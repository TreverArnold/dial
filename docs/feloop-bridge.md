# FeLoop Validation Bridge

DIAL now exposes an explicit FeLoop validation lane for parity and regression work without touching
the live project database.

## Why it exists

The main `dial iterate` and `dial validate` flow remains the supported default on this Mac. The
FeLoop bridge exists so DIAL can:

- export the current DIAL task plus validation pipeline into a FeLoop-compatible workflow
- clone the current `.dial/` state into an isolated lane instead of writing into the live database
- replay that lane through `feloop` for parity checks before any future cutover

This keeps the experimental path real and testable without replacing the installed CLI behavior.

## Commands

Prepare an isolated lane:

```bash
dial feloop prepare-validation
dial feloop prepare-validation --task 12 --lane-root /tmp/dial-feloop-lane
```

Replay the lane through FeLoop:

```bash
dial feloop replay-validation --feloop-bin /path/to/feloop
dial feloop replay-validation --feloop-manifest /path/to/fe-loop/Cargo.toml
```

Both commands support `--format json` for machine-readable output.

## Lane layout

Each lane contains:

- an isolated `.dial/` clone
- `workflow.toml` with the mapped FeLoop task and command validation steps
- `lane.json` with the lane metadata
- `feloop-inspect.stdout.log` / `stderr.log`
- `feloop-validate.stdout.log` / `stderr.log`
- `feloop-replay.stdout.log` / `stderr.log`
- `result.json` for replay runs

## Safety rules

- The lane root must stay outside the live project `.dial/` directory.
- The bridge defaults to a temp location when no `--lane-root` is provided.
- The exported FeLoop policy disables checkpoint creation and restore so the replay stays a
  validation-oriented lane rather than a replacement task runner.
- If DIAL has no explicit validation pipeline, the bridge falls back to `build_cmd` and `test_cmd`.

## Runtime resolution

`dial feloop replay-validation` resolves FeLoop in this order:

1. `--feloop-bin`
2. `--feloop-manifest`
3. `feloop_bin` config key
4. `feloop` on `PATH`
5. `feloop_manifest` config key

This lets you keep the default installed DIAL path untouched while still proving FeLoop-backed
validation locally.
