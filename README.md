# git-sync

`git-sync` mirrors repositories between Git hosting providers when you run it. It does not install daemons, webhooks, timers, or cron jobs.

Supported providers:

- GitHub
- GitLab
- Gitea

The program uses provider APIs to list and create repositories, then uses the local `git` CLI to fetch and push branches and tags.

## Install

```sh
cargo build --release
```

The binary will be at `target/release/git-sync`.

## Configure

Create the config file:

```sh
git-sync config init
```

Or use the interactive wizard, which can create or update the same config file:

```sh
git-sync config wizard
```

The wizard asks for profile or organization URLs, reuses existing credentials when it can, asks for a PAT only when needed, and then shows the sync group before asking whether to add another group.

Example wizard flow:

1. Enter `https://github.com/alice`.
2. Paste a PAT if no existing GitHub credential can access that namespace.
3. Enter `https://git.wonder.land/alice`.
4. Pick the provider if the instance cannot be detected.
5. Paste a PAT if needed.
6. Optionally add a third endpoint for 3-way sync.

PAT quick setup:

- GitHub: open `https://github.com/settings/tokens`, create a classic PAT with `repo` permissions, then copy the token.
- GitLab: open `<base-url>/-/user_settings/personal_access_tokens?name=git-sync&scopes=api`, create the token, then copy it.
- Gitea: open `<base-url>/user/settings/applications`, create a token with repository access, then copy it.

Add sites. Prefer `--token-env` so PATs do not live in shell history or the config file.

```sh
git-sync config site add \
  --name github \
  --provider github \
  --base-url https://github.com \
  --token-env GITHUB_TOKEN

git-sync config site add \
  --name gitea \
  --provider gitea \
  --base-url https://gitea.example.com \
  --token-env GITEA_TOKEN
```

For self-hosted providers, `--base-url` is the web root. API URLs default to:

- GitHub.com: `https://api.github.com`
- GitHub Enterprise: `<base-url>/api/v3`
- GitLab: `<base-url>/api/v4`
- Gitea: `<base-url>/api/v1`

Override with `--api-url` if your instance is different.

Add one or more mirror groups. Endpoints use `SITE:KIND:NAMESPACE`, where kind is `user`, `org`, or `group` depending on the provider.

```sh
git-sync config mirror add \
  --name personal \
  --endpoint github:user:hykilpikonna \
  --endpoint gitea:user:azalea

git-sync config mirror add \
  --name mewolab \
  --endpoint github:org:MewoLab \
  --endpoint gitea:org:MewoLab
```

You can inspect the generated config with:

```sh
git-sync config show
```

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

`git-sync` stores a small ref cache in the work directory. On later runs it first checks each repository with `git ls-remote --heads --tags`; when all endpoints report the same refs as the last successful sync, it skips the full fetch/push pass for that repository.

Use cron or another scheduler for automatic execution:

```cron
*/15 * * * * GITHUB_TOKEN=... GITEA_TOKEN=... /path/to/git-sync sync
```

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

Branch deletion is not propagated. If a branch exists on one endpoint and is missing elsewhere, it is recreated elsewhere. This avoids accidental data loss in a bidirectional mirror.

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
