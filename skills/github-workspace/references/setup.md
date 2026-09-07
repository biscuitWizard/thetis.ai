# Setting up the GitHub App

These steps need a human with admin rights on the account or org. The agent
cannot create an app or install it on a repository — that is the point of the
design, since installation is the consent boundary.

## 1. Create the app

Go to **Settings → Developer settings → GitHub Apps → New GitHub App**
(`https://github.com/settings/apps/new`), or the org equivalent at
`https://github.com/organizations/<org>/settings/apps/new`.

- **Name** — becomes the commit author, as `<slug>[bot]`. Choose something you
  will be happy to see in `git log`.
- **Homepage URL** — anything; it is required but unused.
- **Webhook** — **uncheck Active**. Nothing here listens for webhooks.

## 2. Permissions

Under **Permissions → Repository permissions**, grant only what is needed:

| Permission | Level | Why |
|---|---|---|
| Contents | Read and write | Read files, create commits, move branches |
| Metadata | Read-only | Mandatory; granted automatically |
| Pull requests | Read and write | Open and update PRs |
| Issues | Read and write | Only if issue tools are wanted |
| Administration | Read and write | Only to create repositories in an org |
| Workflows | Read and write | Only to commit changes under `.github/workflows` |

Committing a workflow file without the Workflows permission fails with a 403
that names the path — the one 403 that is not about installation approval.

## 3. Install it

**Install App** in the left sidebar → choose the account or org → choose **All
repositories** or **Only select repositories**.

Selecting specific repositories is the better habit: anything not listed is
invisible to the app, and reads return 404 rather than 403.

## 4. Generate a private key

On the app's **General** page, scroll to **Private keys → Generate a private
key**. A `.pem` downloads; GitHub keeps no copy.

Put it inside the project (the path must resolve within the project root) and
keep it out of git:

```bash
mkdir -p secrets
mv ~/Downloads/your-app.*.private-key.pem secrets/github-app.pem
chmod 600 secrets/github-app.pem
grep -q '^/secrets/' .gitignore || echo '/secrets/' >> .gitignore
```

## 5. Configure

Note the **App ID** from the app's General page — a number, not the client id
(either works, but the App ID is what the docs mean).

Add to `thetis.local.toml`, which is gitignored:

```toml
[tools.git]
app_id = "123456"
private_key_path = "secrets/github-app.pem"
# installation_id = 12345678   # only if the app is installed on several accounts
```

**Which `thetis.local.toml`?** There are two, and both are read. The worktree's
own copy is merged first; the shared one named by `THETIS_LOCAL_CONFIG`
(normally `<project>/thetis.local.toml` at the top level) is merged **last and
therefore wins**. Put the credential in the shared file: it then applies to
every conversation branch instead of vanishing when the worktree changes.

Two things follow, both of which look like bugs and are not:

- A credential in the shared file cannot be overridden from a worktree. If a
  branch-local value seems ignored, check the shared file for the same key.
- `list_config` and `read_config` do not show values that came from an overlay,
  so a correctly configured `app_id` still reads as "no setting named …". Use
  `git-whoami` to test the credential; the config tools cannot see it.

The private key is read by the orchestrator, not by the tool: a tool has no
filesystem import, so `*_path` keys are resolved host-side and inlined as
`*_contents`. Pasting a multi-line PEM into TOML also works via `private_key`,
but the path is far less error-prone.

## 6. Restart and verify

Config is read at startup:

```
restart_orchestrator
```

Then run `git-whoami`. Success prints the app, its slug, the `git config`
lines for the bot identity, every installation, and the reachable repositories —
and confirms an installation token was minted.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `no GitHub credentials configured` | The block was added to `thetis.toml` but the orchestrator was not restarted, or it went in the wrong file. Config is read only at startup, so an edit without a restart shows exactly this. |
| `could not read the private key: … No such file or directory` | A typo, or the key was never saved to that path. Not a permissions problem. |
| `… does not resolve inside the project root` | The path escapes the project. Move the `.pem` under `secrets/`. |
| `the private key could not be parsed` | A PEM pasted inline lost its newlines. Use `private_key_path`. |
| 401 from `/app` | `app_id` belongs to a different app, or the system clock is over a minute fast. |
| `the App has N installations` | Set `installation_id`; the listing in the error names the candidates. |
| Everything works but a repo 404s | The app is installed with a selected list that omits it. |
