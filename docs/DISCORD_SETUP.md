# Discord Alerts + Bot Ops Setup

> Dry-run safe: no private keys in the repo, no mainnet broadcasts from any
> step here. All secrets go through the environment only.

## 1. Create a webhook in a Discord channel

1. Open Discord → pick a server → pick a channel (e.g. `#arb-alerts`).
2. **Edit Channel → Integrations → Webhooks → New Webhook.**
3. Name it e.g. `arb-paper`, select the same channel.
4. **Copy Webhook URL** (shaped like
   `https://discord.com/api/webhooks/<id>/<token>`).
5. NEVER paste that URL into files, public chats, logs, or the repo.

## 2. Export env (your terminal ONLY)

```bash
# Webhook alerts (may be empty = stdout fallback, stays offline)
export DISCORD_WEBHOOK_URL="https://discord.com/api/webhooks/<id>/<token>"

# Bot ops: role + user label (for auditing)
export DISCORD_ROLE="viewer"      # viewer | operator | admin
export DISCORD_USER="your-name"
export AUDIT_CSV="discord-audit.csv"

# Pause drill ONLY (pauser key, not owner/operator!)
# Create once via `cast wallet new`, fund from a faucet if a broadcast is needed.
export PAUSER_KEY="0x..."
export EXECUTOR_ADDRESS="0x..."   # drill executor address (Sepolia)
export RPC_URL="https://sepolia.base.org"  # or your provider URL (from env)
```

> `PAUSER_KEY`, `EXECUTOR_ADDRESS`, and `RPC_URL` are only needed for the
> `pause` command. `status`, `alerts`, and `test-alert` run without any key.

## 3. Example commands

```bash
# Build both binaries
cargo build --bins

# 1) Test alert delivery (safe, no keys; empty URL = print to stdout)
./target/debug/discord-bot test-alert
DISCORD_WEBHOOK_URL="" ./target/debug/discord-bot test-alert

# 2) Status / alerts (read-only, any role)
./target/debug/discord-bot status
./target/debug/discord-bot alerts

# 3) Paper loop + alert on each simulated WIN (default OFF; best-effort)
./target/debug/base-flash-arb --discord-alerts
# without the flag = no delivery at all

# 4) Pause drill (needs operator/admin role + confirmation + pauser env)
export DISCORD_ROLE="operator"
./target/debug/discord-bot pause --confirmed
# success: "pause() sent", then verify:
# cast call "$EXECUTOR_ADDRESS" "paused()(bool)" --rpc-url "$RPC_URL"
```

## 4. `resume` does NOT exist (refused)

```bash
./target/debug/discord-bot resume
# DENIED: `resume`/unpause is not a Discord command.
# Resume = an on-host, OWNER-signed action at a terminal,
# see docs/RUNBOOK_SEPOLIA.md §4 + §6.
```

A full serenity gateway bot later is a drop-in replacement: swap this CLI
for a gateway that calls the same `discord::handle_command` +
`discord_exposed` — the role rules and allowlist stay unchanged.

## 5. Security table: allowed vs forbidden

| Allowed ✅                                          | Forbidden ❌                                                                                             |
|-----------------------------------------------------|----------------------------------------------------------------------------------------------------------|
| `DISCORD_WEBHOOK_URL` via env                       | Webhook URL in files/config/repo/logs                                                                    |
| Discord process holds `PAUSER_KEY` ONLY             | `OWNER_KEY`/`OPERATOR_KEY` in the Discord process (the bot refuses to start)                             |
| `status`, `alerts`, `pause --confirmed` via Discord | `resume`/unpause, allowlist/limit changes, `sweep`, ownership moves via Discord (no such variants exist) |
| `pause` by `operator`/`admin` + `--confirmed`       | `pause` by `viewer` or without `--confirmed`                                                             |
| Audit to `stdout` + CSV (role/command/result only)  | Audit containing keys/URLs/signing payloads                                                              |
| Alerts carry amounts only (<1800 chars, English)    | Alerts carrying secrets, token-bearing URLs, or full RPC URLs                                            |

## 6. Troubleshooting

| Symptom                                                                  | Meaning                                                 | Action                                                                  |
|--------------------------------------------------------------------------|---------------------------------------------------------|-------------------------------------------------------------------------|
| `test-alert` only prints `[discord-stdout-fallback]`                     | `DISCORD_WEBHOOK_URL` is empty                          | Normal for offline tests; export the URL for real delivery              |
| `alert rate-limited, dropped`                                            | >1 alert within 5 seconds                               | Normal: dropped, not queued; the paper loop is unaffected               |
| `discord webhook non-2xx` / `alert post failed`                          | Network/Discord failure                                 | Best-effort: warned only, paper run continues                           |
| `refusing to start: OWNER_KEY ... set`                                   | An owner/operator key is visible in the Discord process | Unset `OWNER_KEY`/`OPERATOR_KEY`, retry (this process is pauser-only)   |
| `pause requires DISCORD_ROLE=operator\|admin`                            | Role too weak for `pause`                               | `export DISCORD_ROLE="operator"` (or `admin`), retry with `--confirmed` |
| `retry with --confirmed`                                                 | Forgot the confirmation flag                            | Add `--confirmed`                                                       |
| `PAUSER_KEY is empty` / `EXECUTOR_ADDRESS is empty` / `RPC_URL is empty` | Drill env not set yet                                   | Export per §2, retry                                                    |
| `resume` denied + runbook pointer                                        | Expected                                                | Unpause on-host with the OWNER key (§4)                                 |

## 7. Repo hygiene (required before commit)

```bash
git status --short        # make sure there is no .env / config/local.toml / URL
rg -i "discord.com/api/webhooks/[0-9]+/" . --glob '!target/**' || echo "clean: no webhook URL"
```

No secrets in this document — only `<id>`/`<token>` placeholders.
