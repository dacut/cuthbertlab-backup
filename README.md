# cuthbertlab-backup

Backs up every repository in <https://github.com/cuthbertLab> to
`s3://cuthbertlab-backup`, once a day, from AWS Lambda.

It is a single Rust binary run as a Lambda function, fanned out per repository
by a Step Functions state machine, with all infrastructure in Terraform.
Repositories are discovered on each run, so new ones are picked up
automatically.

## What is backed up

| Area | What | How |
| --- | --- | --- |
| **Code** | Every ref the repository has: branches, tags, notes, and `refs/pull/*` (the commits of every pull request, even from forks that have since been deleted) | One [git bundle](https://git-scm.com/docs/git-bundle) per repository |
| **Wiki** | The wiki's full git history | A second git bundle |
| **Issues** | Issues, comments, the event history (labeled, closed, renamed, ...), labels, milestones | REST API |
| **Pull requests** | Pull requests, review comments (the inline ones), reviews (approvals and change requests), who merged, linked issues | REST and GraphQL APIs |
| **Discussions** | Discussions with their complete comment and reply trees, categories, polls, accepted answers | GraphQL API |
| **Releases** | Release notes, and every binary attached to a release | REST API, then each file |
| **Attachments** | Images and files dragged into issues, pull requests, comments, discussions and release notes | Found by scanning the text for upload links, then each file |
| **Projects** | Organization Projects (v2), open and closed: fields, views, and every item including draft issues | GraphQL API |
| **Settings** | Repository settings, branches and their protection, rulesets, environments, deployments, Pages, workflows, security advisories, forks, commit comments; with an admin token also collaborators, teams, webhooks, deploy keys, autolinks, and Actions variable and secret *names* | REST API |
| **Organization** | Profile, the repository index, members, teams and their rosters, outside collaborators, rulesets, webhooks | REST API |

API data is stored exactly as GitHub returned it, including fields this
program knows nothing about.

### What is not

- **Git LFS objects.** No cuthbertLab repository uses LFS today. If one starts
  to, its git task will **fail** (after backing up everything else) rather
  than quietly produce a backup with holes in it.
- **Actions run history, logs, artifacts and caches**, and **packages**.
  These are large, short-lived, and reproducible from the code.
- **Secret values.** GitHub never reveals them. Only their names are recorded.
- **Who reacted.** Reaction *counts* are kept (👍 × 3); the list of who
  clicked is not.
- **Stargazer and watcher lists.** They are requested, but GitHub currently
  answers "not found" for them on these repositories, so they are recorded
  as skipped.
- **Anything the token cannot read.** Each `manifest.json` lists what was
  skipped and why. See [the token](#the-github-token).

## How it works

```text
EventBridge Scheduler (daily)
        │
        ▼
Step Functions state machine
        │
        ├─ Plan ─────────── Lambda {"action":"plan"}
        │                   lists the organization's repositories and returns
        │                   one task for the org, plus two per repository
        │
        ├─ Backup (Map) ─── Lambda {"action":"backup","part":"org"}
        │    4 at a time    Lambda {"action":"backup","part":"git","repo":"music21"}
        │                   Lambda {"action":"backup","part":"metadata","repo":"music21"}
        │                   ...
        │
        └─ Check ────────── any task failed?  ──▶ execution FAILS ──▶ SNS email
```

Each task is one Lambda invocation with its own 15-minute budget. A task that
fails is retried twice; if it still fails, it is recorded and **the other
tasks carry on**, and the run as a whole is marked failed. One broken
repository never stops the rest from being backed up.

Three properties are worth knowing about:

**Unchanged data is not re-uploaded.** Before fetching a repository, the git
task compares the remote's refs against a fingerprint stored with the last
bundle; if nothing was pushed, nothing is downloaded. Metadata files carry a
content hash and are rewritten only when they differ. Release binaries and
attachments are transferred once. A typical daily run is therefore small, and
a task that times out resumes where it stopped. A full run over the whole
organization makes roughly 700 GitHub API requests, against a limit of 5,000
an hour.

**Nothing is ever deleted.** The function's IAM role has no delete
permission. The bucket is versioned, so overwriting a file keeps the old one:
if a branch is force-pushed, an issue deleted, or an account compromised,
yesterday's backup is still there. Superseded versions are kept for 90 days,
and the three most recent are kept indefinitely. A repository deleted from
GitHub keeps its last backup forever.

**GitHub's lists are cross-checked.** The REST "list issues" endpoint is
documented to return every issue and pull request. Measured against this
organization, it silently omits hundreds of pull requests. So the issue and
pull request lists are reconciled against the numbers GraphQL reports, and
anything missing is fetched individually. Anything GitHub reports but will
not return is named in a warning.

## Bucket layout

```text
s3://cuthbertlab-backup/github/
├── org/
│   ├── manifest.json                 when, what, and what was skipped
│   ├── org.json
│   ├── repos.json.gz                 index of every repository
│   ├── members.json.gz  teams.json.gz  projects.json.gz  ...
└── repos/<repo>/
    ├── git/
    │   ├── <repo>.bundle             the whole repository
    │   ├── refs.json                 every ref and what it points to
    │   └── manifest.json
    ├── wiki/
    │   ├── <repo>.wiki.bundle        only if the repository has a wiki
    │   └── refs.json
    ├── metadata/
    │   ├── manifest.json             counts, skipped items, warnings
    │   ├── repo.json
    │   ├── issues.json.gz            see the note below
    │   ├── issue_comments.json.gz
    │   ├── issue_events.json.gz
    │   ├── pulls.json.gz
    │   ├── pull_details.json.gz      reviews, merged-by, closing references
    │   ├── pull_review_comments.json.gz
    │   ├── discussions.json.gz       only if discussions are enabled
    │   ├── releases.json.gz
    │   ├── attachments.json.gz       every upload link and where it was stored
    │   └── labels, milestones, branches, rulesets, ...
    ├── releases/<tag>/<file>         release binaries
    └── attachments/<host>/<path>     uploaded images and files
```

Each `.json.gz` is a gzipped JSON array with one element per line, so it
works with `jq` and with `grep`:

```sh
aws s3 cp s3://cuthbertlab-backup/github/repos/music21/metadata/issues.json.gz - \
  | gunzip | jq '.[] | select(.number == 1234)'
```

`issues.json.gz` holds every issue. Because GitHub models a pull request as
an issue with extra data, it also holds the issue view of *some* pull
requests: whichever ones GitHub chose to include (see above). Entries with a
`pull_request` key are pull requests. `pulls.json.gz` is the authoritative,
complete list of those.

## Deploying

You need [Rust](https://rustup.rs), [cargo-lambda](https://www.cargo-lambda.info)
(which needs [Zig](https://ziglang.org) to cross-compile), Terraform 1.7 or
later, and AWS credentials for the account that owns the bucket.

```sh
make init      # terraform init, using terraform/kanga.s3.tfbackend
make deploy    # build the function, then terraform apply
```

The `cuthbertlab-backup` bucket already exists, so Terraform **adopts** it
(via an `import` block) rather than creating it. The first apply turns on
versioning and adds a lifecycle rule and a TLS-only bucket policy; it does
not touch the bucket's contents.

To be emailed when something goes wrong, set `alert_email` (for example in
`terraform/terraform.tfvars`, which is git-ignored) and confirm the
subscription email AWS sends.

### The GitHub token

The token lives in an SSM SecureString parameter that you create yourself.
Terraform is given only its name (`github_token_parameter`) and neither
creates nor reads it, so the token can never enter the Terraform state.
Create it before the first run, and run the same command to rotate it:

```sh
aws ssm put-parameter --overwrite --type SecureString \
  --name /cuthbertlab-backup/github-token \
  --value "ghp_..."
```

A **classic personal access token** with these scopes:

| Scope | Needed for |
| --- | --- |
| `read:project` | Projects. **Required** unless you set `backup_projects = false`. |
| `read:org` | Organization members and teams |
| `repo` | Private repositories, draft releases. Everything public works without it. |

What the token can see is also limited by *who owns it*. Settings such as
webhooks, deploy keys, collaborators and branch protection are readable only
by a repository admin, so a token belonging to an organization owner backs up
the most. A token without those rights still backs up all the content (code,
issues, pull requests, discussions, releases, attachments); the settings it
cannot read are listed under `skipped` in each `manifest.json`.

A token that lacks `read:project` makes the `org` task fail, on purpose: a
permission problem should be noticed when the backup is set up, not when a
project needs restoring.

Tokens expire. When this one does, runs fail and the alert fires; store a new
one with the same command.

Then start a run instead of waiting for the schedule:

```sh
make run
make status
```

The first run uploads everything (about 7 GB for cuthbertLab, most of it
music21's release binaries). If a task runs out of its 15 minutes it is
retried and continues from where it stopped.

## Operating

**Did last night's backup work?** `make status`, or the Step Functions
console. A failed execution names each failed task and its error in the
execution's `cause`. Full detail is in the function's CloudWatch log group,
`/aws/lambda/cuthbertlab-backup`.

**Alerts.** Two things publish to the `cuthbertlab-backup-alerts` SNS topic:

- A run ends as failed, timed out, or aborted.
- No run has fully succeeded for two days. This catches what the first
  cannot: a schedule that stopped firing produces no failure to report.

**Checking a backup.** Each task writes a `manifest.json` saying when it ran
and how many of each thing it stored. For the code:

```sh
aws s3 cp s3://cuthbertlab-backup/github/repos/music21/git/music21.bundle .
git clone --mirror music21.bundle music21.git
git -C music21.git fsck --full
```

## Browsing the backups

The bucket is published, read-only, at <https://cuthbertlab-backup.kanga.org/>
through CloudFront (`terraform/website.tf`).

- A path ending in `/` is a directory listing: HTML by default, or JSON
  with `?Format=json` or `Accept: application/json`. Long directories are
  split into pages of 1,000 entries, with a "Next page" link (in JSON,
  `NextContinuationToken`).
- A directory asked for without its trailing slash redirects to it.
- Any other path is the file. The `.json.gz` collections download under
  their own name; `gunzip` them to read them.

**Everything in the bucket is public**, so the site shows exactly what the
token can see. A token belonging to someone outside the organization sees
only what GitHub shows everyone, and that is all the backup then contains:
`org/members.json.gz` lists public members only, and private projects,
draft releases and admin-only settings are skipped. A token from an
organization member or owner would also back up, and so publish, private
members, private projects, draft releases and any private repositories.
Change the token only with that in mind.

The bucket itself stays private: only the one CloudFront distribution can
read it, and only the current version of each object.

Listings come from a second, small Lambda function in `listing/`, reachable
only through CloudFront. When S3 has no object at a path, CloudFront asks
that function instead, which is how a directory without its trailing slash
gets redirected. Security headers (a strict Content-Security-Policy, HSTS
and so on) are added by CloudFront to every response.

Listings are cached for five minutes and files for an hour, so a run's
results can take that long to show up.

## Restoring

**A repository**, with every ref including pull request heads:

```sh
aws s3 cp s3://cuthbertlab-backup/github/repos/music21/git/music21.bundle .
git clone --mirror music21.bundle music21.git
```

or just `git clone music21.bundle` for an ordinary working copy. To put it
back on GitHub, create an empty repository and push branches and tags
(GitHub does not accept pushes to `refs/pull/*`):

```sh
git -C music21.git push git@github.com:cuthbertLab/music21.git 'refs/heads/*' 'refs/tags/*'
```

**An earlier version** of any file, for instance the bundle from before a
force-push:

```sh
aws s3api list-object-versions --bucket cuthbertlab-backup \
  --prefix github/repos/music21/git/music21.bundle
aws s3api get-object --bucket cuthbertlab-backup \
  --key github/repos/music21/git/music21.bundle --version-id <id> music21.bundle
```

**Issues, pull requests and discussions** are preserved as data. GitHub has
no way to re-create them with their original authors and dates, so there is
no "restore" button; the backup is the record, and the input for whatever
import is appropriate if it ever comes to that.

## Running it locally

The same binary runs from a shell, writing to S3 or to a local directory.
It needs a token in `GITHUB_TOKEN`.

```sh
export GITHUB_TOKEN=$(gh auth token)

cargo run --release -- --org cuthbertLab --local-dir /tmp/backup plan
cargo run --release -- --org cuthbertLab --local-dir /tmp/backup git music21
cargo run --release -- --org cuthbertLab --local-dir /tmp/backup metadata music21
cargo run --release -- --org cuthbertLab --local-dir /tmp/backup org
cargo run --release -- --org cuthbertLab --local-dir /tmp/backup all
```

Replace `--local-dir` with `--bucket cuthbertlab-backup --prefix github` to
write to S3 with your own AWS credentials.

### Tests

```sh
make test            # unit tests; no network, no AWS
make test-emulator   # the S3 and SSM code, against a local emulator
```

`make test-emulator` installs [Moto](https://github.com/getmoto/moto) into
`target/moto/venv` (it needs `python3`), starts it on port 5599, runs the
tests marked `#[ignore]`, and stops it again. Those tests refuse to run
unless `AWS_ENDPOINT_URL` points at this machine, so they cannot touch a real
account. `make moto-start` and `make moto-stop` run the emulator on its own.

Moto is more lenient than AWS: it accepts some requests that S3 rejects. A
pass here shows that data round-trips, not that AWS will accept every
request.

## Configuration

Terraform variables, all with defaults (see `terraform/variables.tf`):

| Variable | Default | Meaning |
| --- | --- | --- |
| `github_org` | `cuthbertLab` | Organization to back up |
| `github_token_parameter` | `/cuthbertlab-backup/github-token` | Existing SSM SecureString parameter holding the token |
| `bucket_name` | `cuthbertlab-backup` | Destination bucket |
| `website_domain` | `cuthbertlab-backup.kanga.org` | Where the bucket is published (see `variables.tf` for the other `website_*` settings) |
| `bucket_prefix` | `github` | Key prefix within it |
| `schedule` / `schedule_timezone` | 03:00 daily, `America/Los_Angeles` | When to run |
| `alert_email` | none | Who to tell |
| `backup_projects` | `true` | Needs `read:project` |
| `backup_attachments` | `true` | |
| `noncurrent_version_retention_days` | `90` | How long superseded versions are kept |
| `noncurrent_versions_always_kept` | `3` | ...and how many are kept regardless |
| `stale_backup_alarm_days` | `2` | Alarm after this long with no success |
| `max_concurrency` | `4` | Tasks run at once |
| `lambda_memory_mb` / `lambda_ephemeral_storage_mb` | `4096` / `8192` | A git mirror needs scratch disk of about twice the repository's size |

## Design notes

**Why not [github-export](https://github.com/mevdschee/github-export)?** It
was evaluated first. It is built to give an AI agent a readable digest of a
repository, not to back one up: it converts a selection of fields to
Markdown and discards the rest, has no support for git data, wikis, release
binaries or attachments, truncates long review and comment threads with a
warning, treats every error as a warning and exits successfully, and keeps
its sync state on local disk. Its code is also all in Go `internal/`
packages, so it cannot be imported as a library.

**Why Rust, and no `git` binary?** Lambda's runtimes do not include git.
Mirroring is done in-process with [gitoxide](https://github.com/GitoxideLabs/gitoxide),
which keeps the function a single static binary deployed as a plain zip,
with no container image or layer to maintain. A fetch into an empty
repository yields one self-contained pack, and a git bundle is exactly a
list of refs followed by such a pack, so the bundle is written directly. The
result was verified with stock git: bundles clone cleanly, pass `git fsck`,
and their refs match `git ls-remote` of the original.

**Why Step Functions?** Lambda's 15-minute limit is fine for any one
repository but not for all of them in sequence. Step Functions also gives
per-task retries, concurrency control against GitHub's rate limit, and a
readable record of every run.

## Cost

For this organization, roughly a dollar a month: about 7 GB in S3 plus
superseded versions, a few minutes of Lambda a day, and one CloudWatch alarm.
