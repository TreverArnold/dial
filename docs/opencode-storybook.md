# OpenCode execution and local Storybook reuse

This fork adds OpenCode to `dial auto-run` and optional project-local Storybook context.
It works with any provider/model configured in OpenCode; no provider credentials belong in DIAL.
The planning wizard retains its existing supported backends. OpenCode support here is for execution.

## OpenCode

Install and authenticate OpenCode, then check `opencode models` for the exact model ID.
In an initialized DIAL project:

```sh
dial config set ai_cli opencode
dial config set opencode_model tokenharbor/deepseek-v4.1-flash:free
dial config set opencode_agent build
dial auto-run --cli opencode --max 1
```

`opencode_model` and `opencode_agent` are optional; empty values use OpenCode defaults.
Each attempt starts a fresh `opencode run` in the project directory and attaches the DIAL task prompt.
Model and agent values are passed as literal process arguments, not interpolated into shell commands.
DIAL does not add `--auto`, resume old sessions, attach to existing servers, or change permissions.
Configure appropriate OpenCode permissions for unattended execution; denied tools can block a task.
OpenCode's ordinary configuration and plugins remain active. Existing DIAL signal handling, validation,
checkpointing and commit behavior still apply. `--max` limits completed tasks, not total attempts.

## Storybook

Use the project's existing Storybook configuration. Set the source/configuration root:

```sh
dial config set storybook_dir frontend
```

This adds instructions to inspect existing components and stories before creating UI, reuse design
tokens, and save new reusable components with source stories. It does not install Storybook, choose
a framework, generate components itself, or share components across projects.

Optionally build Storybook with the project's build command and point to its generated index:

```sh
dial config set storybook_index frontend/storybook-static/index.json
```

The index must contain Storybook's `entries` object. DIAL includes up to 20 story entries ranked by
words in the task description, capped at 6,000 bytes of entries. It reads only local files inside the
project, rejects files over 2 MB, and does not execute Storybook configuration. The generated index
is a discovery hint; workers must inspect source and search beyond the shortlist. Rebuild it when
stories change. Missing or malformed configured paths stop prompt generation rather than silently
omitting the catalog. Set `storybook_index` to an empty string to use source discovery alone.
Set `storybook_dir` to an empty string to disable the integration entirely.

Use required pipeline commands to enforce checks, adapted to your project's scripts:

```sh
dial pipeline add code "npm run test:unit" --order 10
dial pipeline add stories "npm run build-storybook" --order 20
dial pipeline add functionality "npx playwright test" --order 30
```

Steps are required unless `--optional` is supplied. A configured pipeline replaces the legacy
`build_cmd`/`test_cmd` fallback, so include every required check in it. These steps run for each
validated task, not automatically once per feature. For large projects, define appropriate task
boundaries and targeted project scripts. Playwright must start/target the actual application under
test and contain meaningful assertions. Storybook builds alone do not prove UI functionality.

The separate `--browser-check` task flag still requires manual browser-verification recording;
it does not launch Playwright or inspect screenshots. Validation command timeouts in the upstream
runner are not enforced, so use test-runner or external timeouts. The worker's upstream streaming
timeout also depends on output arriving; use an external process deadline for bounded unattended runs.
