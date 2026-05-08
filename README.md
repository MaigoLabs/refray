# git-sync

`git-sync` mirrors repositories between Git hosting providers when you run it. It can run as a one-shot sync command, or as a webhook receiver that triggers one-repo syncs after push events.

Supported providers:

- GitHub
- GitLab
- Gitea
- Forgejo

The program uses provider APIs to list and create repositories, then uses the local `git` CLI to fetch and push branches and tags.
Forgejo uses the same API shape as Gitea.

## Install

```sh
cargo build --release
```

The binary will be at `target/release/git-sync`.

## Configure

Run the interactive configuration wizard:

```sh
git-sync config
```

The wizard creates or updates the config file. It asks for profile or organization URLs, reuses existing credentials when it can, asks for a PAT only when needed, then offers webhook setup. Webhooks are strongly recommended because they sync soon after pushes and greatly reduce the chance of divergent histories.

Example wizard flow:

1. Enter `https://github.com/alice`.
2. Paste a PAT if no existing GitHub credential can access that namespace.
3. Enter `https://git.wonder.land/alice`.
4. Pick the provider if the instance cannot be detected.
5. Paste a PAT if needed.
6. Optionally add a third endpoint for 3-way sync.
7. Enable webhooks and enter the public webhook URL.

PAT quick setup:

- GitHub: open `https://github.com/settings/tokens`, create a classic PAT with `repo` permissions, then copy the token.
- GitLab: open `<base-url>/-/user_settings/personal_access_tokens?name=git-sync&scopes=api,write_repository`, select `api` and `write_repository`, create the token, then copy it.
- Gitea: open `<base-url>/user/settings/applications`, create a token with repository access, then copy it.
- Forgejo: open `<base-url>/user/settings/applications`, create a token with repository access, then copy it.

There are no separate configuration mutation commands. If you do not want to use the wizard, edit the config TOML directly; see the example config below. For self-hosted providers, `base_url` is the web root. API URLs default to:

- GitHub.com: `https://api.github.com`
- GitHub Enterprise: `<base-url>/api/v3`
- GitLab: `<base-url>/api/v4`
- Gitea: `<base-url>/api/v1`
- Forgejo: `<base-url>/api/v1`

Set `api_url` in the TOML if your instance is different.

## Sync

Run all configured mirror groups:

```sh
git-sync sync
```

Run one group:

```sh
git-sync sync --group personal
```

Preview commands without writing to Git remotes:

```sh
git-sync sync --dry-run
```

Sync only repositories whose names match a regex:

```sh
git-sync sync --repo-pattern '^(foo|bar)-'
```

Retry only repositories that failed during the previous non-dry-run sync:

```sh
git-sync sync --retry-failed
```

Control repo-level parallelism:

```sh
git-sync sync --jobs 8
```

While jobs run, the bottom of the terminal shows one live status line per worker. When a repository finishes, its detailed log is printed as one complete block above those status lines. The default is 4 workers; use `--jobs 1` for serial sync.

`git-sync` stores a small ref cache in the work directory. On later runs it first checks each repository with `git ls-remote --heads --tags`; when all endpoints report the same refs as the last successful sync, or the existing local bare mirror cache already has those refs, it skips the full fetch/push pass for that repository.

Use cron or another scheduler for automatic execution:

```cron
*/15 * * * * GITHUB_TOKEN=... GITEA_TOKEN=... /path/to/git-sync sync
```

## Webhooks

Webhook mode reduces the window for divergent commits by syncing a repository immediately after a provider sends a push event. It is still conservative: if two endpoints receive independent commits before webhook sync catches up, the normal divergence rules still apply.

The interactive wizard can configure webhooks for you. During setup it starts a temporary test listener on `127.0.0.1:8787`, asks for the public URL, checks that the URL is reachable from the current machine, creates a webhook secret, and can enable periodic full syncs while `git-sync serve` is running.

Example config:

```toml
[webhook]
install = true
url = "https://mirror.example.com/webhook"
secret = { value = "generated-secret" }
full_sync_interval_minutes = 60
reachability_check_interval_minutes = 15
```

Start the receiver:

```sh
git-sync serve \
  --listen 127.0.0.1:8787
```

Expose that listener with your reverse proxy or tunnel, then install repository webhooks. If `[webhook]` is configured, the URL and secret can come from config:

```sh
git-sync webhook install
```

Manual `webhook install` always checks the selected repositories on the provider and repairs or records the hook state. To install or repair one repository exactly:

```sh
git-sync webhook install important-repo
```

You can also pass them explicitly:

```sh
git-sync webhook install \
  --url https://mirror.example.com/webhook \
  --secret-env GIT_SYNC_WEBHOOK_SECRET
```

Useful install filters:

```sh
git-sync webhook install \
  --url https://mirror.example.com/webhook \
  --secret-env GIT_SYNC_WEBHOOK_SECRET \
  --group personal \
  --repo-pattern '^important-'
```

The receiver accepts `POST /` and `POST /webhook`. It verifies GitHub/Gitea HMAC SHA-256 signatures and GitLab webhook tokens, then queues `git-sync sync --group <group> --repo-pattern '^<repo>$'` internally. Duplicate events for the same group/repo are coalesced while a job is queued or running. Sync jobs are serialized inside the receiver so the local ref and failure caches stay consistent.

When `[webhook].install = true`, normal `git-sync sync` also checks webhook installation status and installs missing webhooks for repositories that have not been recorded yet. Installation status is stored in `webhook-state.toml` under the work directory.

To uninstall webhooks previously installed by `git-sync`:

```sh
git-sync webhook uninstall
```

Manual `webhook uninstall` checks repositories on the provider instead of trusting only local state. To uninstall one repository exactly:

```sh
git-sync webhook uninstall important-repo
```

To move installed hooks to a new public URL, use `webhook update`. It removes hooks matching the current configured `[webhook].url`, installs the new URL, updates `[webhook].url` in the config, and refreshes local webhook state:

```sh
git-sync webhook update --url https://new.example.com/webhook
```

Serve can also run periodic full syncs. The interval can be configured in `[webhook].full_sync_interval_minutes` or overridden at startup:

```sh
git-sync serve --full-sync-interval-minutes 30
```

If `[webhook].reachability_check_interval_minutes` is configured, `serve` periodically checks that the public webhook URL is still reachable and logs a warning when it is not.

## Sync Semantics

Each mirror group is treated as a set of equivalent namespaces. Repositories are matched by repository name across all endpoints.

For every repository name found in any endpoint, `git-sync` will:

1. Create missing repositories on the other endpoints when `create_missing = true`.
2. Fetch all branches and tags from each existing endpoint into a local bare mirror cache.
3. Compare branch tips across endpoints.
4. Push the winning branch tip to every endpoint.

Branch conflict handling is intentionally conservative:

- If all endpoints agree on a branch tip, that tip is pushed everywhere.
- If one branch tip is a descendant of the others, the descendant wins and is pushed everywhere.
- If branch tips diverged, that branch is skipped and reported.
- If `allow_force = true` or `git-sync sync --force` is used, a diverged branch chooses the newest commit timestamp and force-pushes it.

Branch deletion is propagated only when it is safe to infer intent. If a branch existed on every endpoint in the previous successful sync, then disappears from one endpoint while the remaining endpoints still have the previous tip, `git-sync` deletes it from the remaining endpoints instead of recreating it. If the branch was deleted on one endpoint but changed elsewhere, it is treated as a conflict and skipped.

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

Mirroring issues and pull requests is possible, but it is not the same kind of operation as mirroring Git branches.

Repository Git data has a shared protocol and object model. Issues and pull requests are provider-specific application data. GitHub, GitLab, and Gitea have different fields, permissions, labels, milestones, users, review states, CI metadata, cross-links, attachments, reactions, and webhook/event histories.

A practical implementation should be designed as a separate feature with explicit tradeoffs:

- **Issues:** feasible to copy title, body, state, labels, assignees by mapping usernames, milestones, and labels. Comments can be copied, but original authors and timestamps usually need to be represented in the comment body unless the target API supports impersonation.
- **Pull requests / merge requests:** feasible to copy open PR metadata and comments, but the source and target branches must already exist on the target. Review approvals, check statuses, merge queues, and provider-specific refs do not map cleanly.
- **Bidirectional sync:** much harder than one-time migration. You need durable external IDs, per-provider mapping tables, conflict policy for edits on both sides, deletion/close policy, and rate-limit handling.

Recommended path: keep Git mirroring in this tool's core sync loop, then add an optional `sync-issues` feature with a local state database and provider-specific mappers. Start with one-way issue copy, then add comments, then consider bidirectional updates only after identity and conflict rules are explicit.
