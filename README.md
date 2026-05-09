# refray

A tool to keep your repos in sync across all git platforms, while being able to work from everywhere all at once. 

Created becasue github is so unusable and [unreliable](https://red-squares.cian.lol/) and I want to leave, but I don't want to leave the community behind.

- **∞-side sync**: Sync between any number of hosted/self-hosted git accounts/orgs/groups
- **read-write mirrors**: Make changes from any provider, and the changes will sync to the others
- **webhook support**: Sync right after push, reduce potential divergence window
- **conflict handling**: Rebase or open pull requests when two platforms diverge
- **tracks deletions**: Delete branches/repos across platforms when they are deleted from one platform
- **selective sync**: Sync subset of repos by regex white/black list, or by private/public visibility

Supported platforms: GitHub, GitLab, Gitea, Forgejo

> [!NOTE]
> Meow

## Install

### Option 1. Install from source

1. Install rust cargo if you don't have it: https://rustup.rs
2. `cargo install refray`

### Option 2. Download binary

Go to the [releases page](https://github.com/MaigoLabs/refray/releases), find the latest release, and download the appropriate binary for your platform.

### Option 3. Docker Compose

```sh
docker compose run --rm refray config

# Start the webhook receiver as a service
docker compose up -d --build

# If you want to edit config manually:
docker compose run --rm --entrypoint nano refray /data/config/refray/config.toml
```

## Usage

### 1. Configure

Run the interactive configuration wizard:

```sh
refray config
```

<details><summary>Example Config</summary>

```toml
jobs = 8

[[sites]]
name = "github"
provider = "github"
base_url = "https://github.com"
token = { env = "GITHUB_TOKEN" }

[[sites]]
name = "gitea"
provider = "gitea"
base_url = "https://gitea.example.com"
token = { env = "GITEA_TOKEN" }

[[mirrors]]
name = "personal"
sync_visibility = "all"
repo_whitelist = ["^important-"]
repo_blacklist = ["-archive$"]
create_missing = true
visibility = "private"
allow_force = false
conflict_resolution = "auto_rebase_pull_request"

[[mirrors.endpoints]]
site = "github"
kind = "user"
namespace = "hykilpikonna"

[[mirrors.endpoints]]
site = "gitea"
kind = "user"
namespace = "azalea"
```

</details>

### 2. One-time Sync

Run all configured mirror groups:

```sh
refray sync
```

<details><summary>Sync options</summary>

Run one group:

```sh
refray sync --group personal
```

Preview commands without writing to Git remotes:

```sh
refray sync --dry-run
```

Sync only repositories whose names match a regex:

```sh
refray sync --repo-pattern '^(foo|bar)-'
```

For persistent per-group filters, set `repo_whitelist` and/or `repo_blacklist` in the config instead. `--repo-pattern` is an extra one-off filter applied on top of the group config.

Retry only repositories that failed during the previous non-dry-run sync:

```sh
refray sync --retry-failed
```

Control parallelism for sync, serve, and webhook commands in config:

```toml
jobs = 8
```

</details>

### 3. Service & Webhooks

You can run `refray` as a service that listens for webhook events and runs full sync periodically. This is the recommended way to run `refray`.

> [!NOTE]
> If you want to use webhooks, you need to expose port 8787 to a public URL that can be accessed by the git provider.<br>
> This can be done using e.g. port forwarding, reverse proxy, cloudflare tunnel, or tailscale funnel.

Start the service (to sync on push and also do full sync periodically):

```sh
refray serve
```

Install webhooks on all repos (with the URL in config):

```sh
refray webhook install
```

To uninstall webhooks previously installed by `refray`:

> [!WARNING]
> If you want to stop using `refray`, make sure you run this! Otherwise, all of your repos will keep trying to send webhooks to the URL.

```sh
refray webhook uninstall
```

By default, uninstall uses `[webhook].url` from your config. To remove hooks for a previous URL, pass it explicitly:

```sh
refray webhook uninstall https://old.example.com/webhook
```

To move installed hooks to a new public URL, use `webhook update`. It removes hooks matching the current configured `[webhook].url`, installs the new URL, updates `[webhook].url` in the config, and refreshes local webhook state:

```sh
refray webhook update https://new.example.com/webhook
```

## Sync Semantics

Each mirror group is treated as a set of equivalent namespaces. Repositories are matched by repository name across all endpoints.

Set `sync_visibility = "all"`, `"private"`, or `"public"` on a mirror group to choose which repository visibility is included in that group. `visibility` still controls the visibility used when `refray` creates a missing repository.

Set `repo_whitelist = ["..."]` and/or `repo_blacklist = ["..."]` on a mirror group to filter repository names with regular expressions. An empty whitelist includes all repository names, and blacklist matches are excluded after whitelist matches. These name filters are independent from `sync_visibility`; both must match for a repository to be synced.

For every repository name found in any endpoint, `refray` will:

1. Create missing repositories on the other endpoints when `create_missing = true`.
2. Fetch all branches and tags from each existing endpoint into a local bare mirror cache.
3. Compare branch tips across endpoints.
4. Push the winning branch tip to every endpoint.

Branch conflict handling is intentionally conservative:

- If all endpoints agree on a branch tip, that tip is pushed everywhere.
- If one branch tip is a descendant of the others, the descendant wins and is pushed everywhere.
- If branch tips diverged, `conflict_resolution` controls what happens.
- If `allow_force = true` or `refray sync --force` is used, a diverged branch chooses the newest commit timestamp and force-pushes it.

Conflict resolution strategies are configured per mirror group:

- `fail`: fail the repository sync when branch tips diverge.
- `auto_rebase`: rebase divergent commits in endpoint order into one branch history, push fast-forward updates normally, and force-push only endpoints whose original tip was rewritten. If rebase hits a file conflict, fail.
- `pull_request`: push temporary `refray/conflicts/...` branches and open provider pull requests/merge requests so a person can resolve the divergence.
- `auto_rebase_pull_request`: try `auto_rebase` first, then fall back to pull requests if rebase hits a conflict.

When a previously opened conflict pull request is merged, the next sync sees the merged branch as the winning tip, pushes it to the other endpoints, and closes stale `refray/conflicts/...` pull requests for that branch.

Repository and branch deletion are propagated only when it is safe to infer intent. If a repository existed on every endpoint in the previous successful sync, then disappears from one endpoint while the remaining endpoints still have the previous synced refs, `refray` deletes it from the remaining endpoints instead of recreating it. If the repository was deleted everywhere, `refray` removes its saved sync state. If the repository was deleted on one endpoint but changed elsewhere, it is treated as a conflict and skipped.

Branch deletion follows the same rule at branch scope: if a branch existed on every endpoint in the previous successful sync, then disappears from one endpoint while the remaining endpoints still have the previous tip, `refray` deletes it from the remaining endpoints instead of recreating it. If the branch was deleted on one endpoint but changed elsewhere, it is treated as a conflict and skipped.

Tags are fetched into provider-specific cache refs and pushed only when the tag object agrees across providers or exists on one side. Divergent tags are skipped and reported. Tag deletion is not propagated.

## Testing

Run the normal, non-destructive test suite:

```sh
cargo test
```

The sequential live e2e test is ignored by default because it creates and deletes repositories on real provider accounts. Configure four token/username pairs in `.env` or the process environment:

```sh
GH_USER=...
GH_TOKEN=...
GL_USER=...
GL_TOKEN=...
GT_USER=...
GT_TOKEN=...
FO_USER=...
FO_TOKEN=...
```

Optional base URL overrides are `GL_BASE_URL`, `GT_BASE_URL` or `GITEA_BASE_URL`, and `FO_BASE_URL` or `FORGEJO_BASE_URL`. The Gitea and Forgejo defaults are `https://gitea.com` and `https://codeberg.org`.

Run the destructive e2e test explicitly:

```sh
REFRAY_E2E_ALLOW_DESTRUCTIVE=1 \
  cargo test --test sequential -- --ignored --test-threads=1 --nocapture
```

By default cleanup only deletes repositories named `refray-e2e-*`. To start by deleting every owned repository visible to the configured accounts, set `REFRAY_E2E_CLEAR_ALL_REPOS=DELETE_ALL_OWNED_REPOS`. Provider skips (`REFRAY_E2E_SKIP_GITHUB`, `REFRAY_E2E_SKIP_GITLAB`, `REFRAY_E2E_SKIP_GITEA`, `REFRAY_E2E_SKIP_FORGEJO`) and `REFRAY_E2E_ALLOW_PARTIAL=1` are available for local debugging, but the full support check should run with all four providers.

## Issues and Pull Requests

Issues and pull requests are not mirrored.
