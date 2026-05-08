# refray

A tool to keep your repos in sync across all git platforms, while being able to work from everywhere all at once. 

Created becasue github is so unusable and unreliable and I want to leave, but I don't want to leave the community behind.

- **∞-side sync**: Sync between any number of hosted/self-hosted git accounts/orgs/groups
- **read-write mirrors**: Make changes from any provider, and the changes will sync to the others
- **webhook support**: Sync right after push, reduce potential divergence window
- **conflict handling**: Rebase or open pull requests when two platforms diverge

Supported platforms: GitHub, GitLab, Gitea, Forgejo

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

## Configure

Run the interactive configuration wizard:

```sh
refray config
```

## One-time Sync

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

Retry only repositories that failed during the previous non-dry-run sync:

```sh
refray sync --retry-failed
```

Control repo-level parallelism:

```sh
refray sync --jobs 8
```

</details>

## Service & Webhooks

You can run `refray` as a service that listens for webhook events and runs full sync periodically. This is the recommended way to run `refray`.

If you want to use webhooks, you need to expose port 8787 to a public URL that can be accessed by the git provider (e.g. using port forwarding, reverse proxy, or cloudflare tunnel). 

Start the receiver:

```sh
refray serve
```

Expose that listener with your reverse proxy or tunnel, then install repository webhooks. If `[webhook]` is configured, the URL and secret can come from config:

```sh
refray webhook install
```

Useful install filters:

```sh
refray webhook install \
  --url https://mirror.example.com/webhook \
  --secret-env REFRAY_WEBHOOK_SECRET \
  --group personal \
  --repo-pattern '^important-'
```

The receiver accepts `POST /` and `POST /webhook`. It verifies GitHub/Gitea HMAC SHA-256 signatures and GitLab webhook tokens, then queues `refray sync --group <group> --repo-pattern '^<repo>$'` internally. Duplicate events for the same group/repo are coalesced while a job is queued or running. Sync jobs are serialized inside the receiver so the local ref and failure caches stay consistent.

When `[webhook].install = true`, normal `refray sync` also checks webhook installation status and installs missing webhooks for repositories that have not been recorded yet. Installation status is stored in `webhook-state.toml` under the work directory.

To uninstall webhooks previously installed by `refray`:

```sh
refray webhook uninstall
```

Manual `webhook uninstall` checks repositories on the provider instead of trusting only local state. To uninstall one repository exactly:

```sh
refray webhook uninstall important-repo
```

To move installed hooks to a new public URL, use `webhook update`. It removes hooks matching the current configured `[webhook].url`, installs the new URL, updates `[webhook].url` in the config, and refreshes local webhook state:

```sh
refray webhook update --url https://new.example.com/webhook
```

Serve can also run periodic full syncs. The interval can be configured in `[webhook].full_sync_interval_minutes` or overridden at startup:

```sh
refray serve --full-sync-interval-minutes 30
```

If `[webhook].reachability_check_interval_minutes` is configured, `serve` periodically checks that the public webhook URL is still reachable and logs a warning when it is not.

## Sync Semantics

Each mirror group is treated as a set of equivalent namespaces. Repositories are matched by repository name across all endpoints.

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

Branch deletion is propagated only when it is safe to infer intent. If a branch existed on every endpoint in the previous successful sync, then disappears from one endpoint while the remaining endpoints still have the previous tip, `refray` deletes it from the remaining endpoints instead of recreating it. If the branch was deleted on one endpoint but changed elsewhere, it is treated as a conflict and skipped.

Tags are fetched into provider-specific cache refs and pushed only when the tag object agrees across providers or exists on one side. Divergent tags are skipped and reported. Tag deletion is not propagated.

## Example Config

```toml
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

## Issues and Pull Requests

Issues and pull requests are not mirrored.
