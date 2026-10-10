# Residential-IP crawling with a self-hosted Mac runner

GeoSweep's GitHub-hosted crawler has encountered HTTP 429 around 0.1–0.2
requests/second. Moving the *same job* to your Mac sends search traffic
through your home connection instead of an Actions datacenter. **It may or
may not improve throughput.** Throttling may be account-wide, or keyed by
other factors in addition to source IP.

## Why a self-hosted GitHub Actions runner?

- Existing `GC_USER` / `GC_PASS` repository secrets stay in GitHub
  Actions; don't place passwords in a repo file or shell command.
- Local and hosted jobs share the `geosweep-daily` concurrency group.
  Only one runner restores and updates `db-latest` at a time.
- Hosted Linux stays the default until you explicitly switch.
- Checkpointing, validation, release publishing and seven dated backups
  work the same way on either runner.
- Existing hosted learned-rate key: `adaptive_learned_rate_rps`.
  New local key: `adaptive_learned_rate_rps_local`. The first home run
  starts at **1 request/second**, adapting to 429s. Its rate is then
  remembered independently of datacenter throttling.

## Register the Mac once (after merging this PR)

Requires an Apple Silicon Mac, Homebrew, a stable home connection, and
`gh` authenticated with admin permission on the **private** repository.
Use **fish**:

```fish
set repo adventure-labs-browser/geosweep
set runner_dir "$HOME/.local/share/geosweep-actions-runner"
mkdir -p "$runner_dir"
chmod 700 "$runner_dir"
cd "$runner_dir"

# Download the latest official macOS ARM64 runner.
set tag (gh api repos/actions/runner/releases/latest --jq .tag_name)
set ver (string replace -r '^v' '' "$tag")
set archive "actions-runner-osx-arm64-$ver.tar.gz"
gh release download "$tag" --repo actions/runner --pattern "$archive" --clobber
tar -xzf "$archive"
rm "$archive"

# Registration tokens are short-lived. Never save or print them.
set -l registration_token (gh api --method POST "repos/$repo/actions/runners/registration-token" --jq .token)
./config.sh --url "https://github.com/$repo" --token "$registration_token" \
    --name geosweep-home-mac --labels geosweep --unattended

# Register and start the official per-user launchd service.
./svc.sh install
./svc.sh start
```

The local job installs Playwright in its own `.venv`, checks for Homebrew
`gh`, `zstd` and GNU `gtimeout`, and installs missing tools. Rust comes
from the workflow's existing toolchain action. Running your own Actions
runner lets this private repo's workflows execute code with your macOS user
permissions; secure the account and do not accept untrusted workflow edits.

The Mac must be awake, connected and not sleeping when jobs are scheduled.
The runner must report `self-hosted`, `macOS`, `ARM64`, `geosweep` labels.
Check **Settings → Actions → Runners** in the repo.

## Measure local throughput once, without changing the schedule

Once merged and the runner is online, dispatch from fish:

```fish
gh workflow run daily.yml --repo adventure-labs-browser/geosweep --ref main -f runner=local -F crawl_minutes=45
```

This will queue behind a running hosted job via GitHub's shared concurrency
group. It is a normal checkpointed **45-minute pilot crawl**, plus setup
and publishing, not a separate archive. Omit the minutes option for a normal
4h45m run (maximum 285 minutes). Compare `rate limiter` logs,
`refresh: saved learned API rate`, HTTP 429s, and **unique caches
added per hour** against a GitHub-hosted run. One successful 1/s burst
is not proof of sustained performance.

If residential throughput is reliably higher, switch the *scheduled*
job to the Mac:

```fish
gh variable set GEOSWEEP_RUNNER --repo adventure-labs-browser/geosweep --body local
```

To restore the scheduled Linux runner:

```fish
gh variable delete GEOSWEEP_RUNNER --repo adventure-labs-browser/geosweep
```

You may still manually dispatch `-f runner=hosted` after switching.
Do **not** switch schedules before the Mac runner is online. If offline,
the local job remains queued until it comes back or is canceled.

## Safety

- If the rolling release download fails, the workflow now fails *closed*
  instead of starting a new empty database and potentially overwriting the
  checkpoint on publication.
- The existing bounded crawl gracefully cancels in-flight searches,
  releases per-page checkpoints, validates the SQLite database, and
  publishes only afterward.
- The self-hosted runner's workspace contains browser authentication state
  while a job runs. Keep the Mac user private and don't publish this
  workspace. Storage-state files and `.venv` are gitignored.
- Do **not** run a separate `cargo run` crawl at the same time; it would
  bypass the Actions concurrency lock and could race on `db-latest`.
- The workflow does not automatically switch your existing schedule or
  install/start a runner merely by merging this PR.
