---
description: Commit staged changes with a Conventional Commits message
---

Commit staged changes using Conventional Commits 1.0.0. Every commit message must follow the format and length rules in
"Commit Message Rules" below. Validation is delegated to git hooks, which are the single source of truth:
`.githooks/commit-message` (message format and length) and `.githooks/pre-commit` (secret and credential scan).

Optional user input: $ARGUMENTS
If provided, treat it as extra context for the message (intent, scope, or issue reference). The format and length rules
still apply.

## Step 1: Check for Staged Changes

!`git status --short` !`git diff --staged --stat`

If no staged changes, respond:

```
Nothing is staged. Stage files first with `git add <files>` or `git add .`, then run /git-commit again.
```

Stop.

## Step 2: Scan for Anomalies

**Large files (>1 MB):** !`git diff --staged --name-only | xargs -I{} find {} -maxdepth 0 -size +1M 2>/dev/null`

**Potential secrets / credentials (pre-commit hook, values are never printed):** !
`bash .githooks/pre-commit 2>&1; echo "exit=$?"`

Interpreting the secret scan:

- `exit=0`: nothing found, report "None".
- `exit=1`: the output lists each finding as `[filename]` or `[content]` with file and line numbers. Warn the user
  clearly and report them in the Anomaly Check. The `pre-commit` hook will also block the commit in Step 5, so recommend
  unstaging or fixing the files (`git restore --staged <file>`), or adding a `secret-scan:ignore` comment on a line that
  is a confirmed false positive. Do not stage or unstage anything yourself.
- If `.githooks/pre-commit` does not exist, tell the user to add it (see "Hook Setup") and report the secret scan as
  "Not run".
- Never use `SKIP_SECRET_CHECK=1` unless the user explicitly says they reviewed the findings and asks to bypass the
  check.

Warn the user if any other issues are found.

## Step 3: Review Staged Diff

!`git diff --staged` !`git log --oneline -5`

Use the recent log to match the project's existing scopes and wording style.

## Step 4: Present Summary and Proposed Commit Message

Draft the message following "Commit Message Rules", then validate it with the hook script (see "Validate Before
Presenting") before showing it.

Present:

```
## Staged Changes Summary
[1–2 sentence description]

## Files Staged

| File | Status | Description |
|------|--------|-------------|
| [file path] | Added / Modified / Deleted | [one-line description] |

## Anomaly Check
- Large files (>1 MB): [list or "None"]
- Potential secrets / credentials in filenames: [list or "None"]
- Secret-like patterns in content (file and line numbers only): [list or "None"]

## Proposed Commit Message
[Full conventional commit message]

Subject length: [N]/75 characters
```

If the staged changes mix unrelated concerns (for example a feature and an unrelated refactor), say so and recommend
splitting into separate commits. Do not unstage or stage anything yourself; let the user decide.

Use the question tool for confirmation:

- **Proceed** — commit with message
- **Cancel** — abort
- **Edit message** — user provides revised message, then validate it again and re-confirm

Wait for response. Cancel: inform user. Edit: collect the revised message, run it through the same validation (reject
and explain if it fails, proposing a compliant version), then re-confirm.

## Step 5: Commit

1. Check signing config and hook setup:
   !`git config --get commit.gpgsign` !`git config --get gpg.format` !`git config --get user.signingkey` !
   `git config --get core.hooksPath`
   Never bypass signing — use the user's config as-is.
   If `core.hooksPath` is not `.githooks`, warn the user that the hooks will not run automatically on `git commit` (the
   checks in Step 2 and Step 4 still applied) and show the setup commands from "Hook Setup".

2. Commit with the approved message, passed through stdin so multi-line messages and quotes are preserved exactly:

```bash
git commit -F - <<'EOF'
<approved message>
EOF
```

3. If hanging on passphrase/key entry: terminate, warn the user, do not retry:

```
Commit timed out waiting for passphrase/key entry. The commit was not created.
Unlock your key agent (e.g. `gpg-agent`, `ssh-agent`) before retrying.
```

4. If a commit hook rejects (including `pre-commit` and `commit-message`): show full hook output, ask to fix/retry or abort,
   do not retry automatically. For a `pre-commit` rejection, never retry with `SKIP_SECRET_CHECK=1` on your own.

5. On success, report:

```
Committed: <commit hash> — <commit subject line>
```

## Commit Message Rules

Always use Conventional Commits 1.0.0:

```
<type>[optional scope][!]: <description>

[optional body]

[optional footer(s)]
```

**Type** (choose the one that matches the primary intent of the staged diff):

| Type       | Use for                               |
|------------|---------------------------------------|
| `feat`     | A new feature or capability           |
| `fix`      | A bug fix                             |
| `docs`     | Documentation only                    |
| `style`    | Formatting only; no logic change      |
| `refactor` | Restructuring with no behavior change |
| `perf`     | A performance improvement             |
| `test`     | Adding or correcting tests            |
| `build`    | Build system or dependency changes    |
| `ci`       | CI configuration and scripts          |
| `chore`    | Maintenance that fits no other type   |
| `revert`   | Reverting a previous commit           |

**Scope**: optional, a lowercase noun for the affected area (for example `auth`, `api`, `deps`). Reuse scopes already
seen in `git log`; omit it when the change spans the whole project.

**Description**:

- Imperative, present tense ("add", "fix", "remove"), not "added" or "adds"
- Start with a lowercase letter (except proper nouns and acronyms); no trailing period
- Say what changed and, if it fits, why; do not list files; no emojis

**Breaking changes**: add an exclamation mark before the colon (for example `feat(api)!: remove v1 endpoints`) or a
`BREAKING CHANGE: <explanation>` footer (uppercase); either one is enough. Only when the staged diff actually breaks an
API or behavior.

**Body**: optional. Add one only when the reason or context is not obvious from the subject. Separate it from the
subject with one blank line; explain what and why, not how.

**Footers**: only when real (for example `BREAKING CHANGE:`, or `Refs: #123` if the user gave an issue reference).

### Length Limit: 75 Characters

- The **subject line** (the whole first line, including type, scope, the exclamation mark if present, the colon, and the
  space) must be **at most 75 characters**
- Every **body and footer line** must also be **at most 75 characters**; wrap longer text onto new lines
- If the subject does not fit: use shorter wording, a shorter scope, or move detail into the body. Never truncate
  mid-word, never use an ellipsis, and never drop the type
- This is a hard limit. Do not present or commit a message that exceeds it

### Validate Before Presenting

Validation lives in `.githooks/commit-message`. Do not re-implement the checks here; run the hook script against the drafted
message (quoted heredoc keeps quotes and special characters safe):

```bash
TMP=$(mktemp)
cat > "$TMP" <<'EOF'
<drafted message>
EOF
bash .githooks/commit-message "$TMP"; echo "exit=$?"
rm -f "$TMP"
```

- `exit=0`: the message is valid, present it.
- Non-zero: read the error output, fix the message, and re-run until it passes.
- If `.githooks/commit-message` does not exist, tell the user to add it (see "Hook Setup") and stop. Do not fall back to
  unvalidated commits.

The hook checks the format of the subject, the blank line after it, and the 75-character limit on every line. Wording
rules that cannot be checked mechanically (imperative mood, lowercase start, usefulness of the description) remain your
responsibility.

Report the subject length in the summary. If the staged changes are too large to describe in one subject, say so and
suggest splitting the commit.

## Hook Setup

One-time setup per clone (the hook files are committed to the repo under `.githooks/`):

```bash
chmod +x .githooks/commit-message .githooks/pre-commit
git config core.hooksPath .githooks
```

## Constraints

- Do not pass `--no-verify`, `--no-gpg-sign`, or any skip flags unless the user explicitly requests it.
- Do not stage additional files — only commit what is already staged.
- Do not push — commit-only.
- Never commit a message that fails the hook validation.
- Do not set `SKIP_SECRET_CHECK=1` unless the user explicitly requests it after reviewing the findings.