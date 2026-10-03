//! Control-plane stub (Discord or similar transport).
//!
//! Architecture: this module is PURE decision logic with no network, no
//! secrets, and no handle on the trading hot path. The real transport
//! (serenity/poise or an HTTP webhook bridge) runs as a SEPARATE process and
//! talks to the bot through a narrow channel/DB row. Discord latency or
//! outages must never affect trading or the kill switch.
//!
//! Key custody (non-negotiable, see `docs/RUNBOOK_SEPOLIA.md`):
//! - The Discord transport holds the PAUSER key ONLY (pause + read-only
//!   status/alerts). It NEVER holds the owner key, the operator key, or any
//!   signing key capable of moving funds.
//! - FORBIDDEN via Discord (no transport wiring, no exceptions):
//!   `unpause` / resume, allowlist changes, limit changes, `sweep` /
//!   `sweepETH`, ownership changes. Resume after a pause is an ON-HOST,
//!   OWNER-signed action performed by a human at a terminal, never a chat
//!   command. `ControlCommand::Resume` exists in the pure logic below ONLY
//!   for that on-host channel; [`discord_exposed`] returns false for it so a
//!   transport cannot wire it by accident.
//!
//! Rules enforced here:
//! - Read-only by default: status/PnL/alerts, any role.
//! - State-changing commands (pause/resume) require Operator+ and explicit
//!   `confirmed=true`.
//! - NOTHING here can move funds, reveal secrets, or raise a risk limit
//!   above its hard-coded maximum.
//! - Every command produces an audit line (returned to the caller for the
//!   append-only log).

use thiserror::Error;

/// Discord-side role (mapped from the guild's role allowlist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

/// Commands the control plane understands. Deliberately small: no
/// fund-moving, no limit-raising commands exist at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    /// Read-only: health, mode, PnL snapshot.
    Status,
    /// Read-only: recent alerts.
    Alerts,
    /// Halt new attempts (dry-run flags only in this stub).
    Pause { confirmed: bool },
    /// Resume after a pause (never clears the manual kill switch).
    Resume { confirmed: bool },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ControlError {
    #[error("role not authorized for state-changing command")]
    Unauthorized,
    #[error("confirmation required: re-issue with confirmed=true")]
    ConfirmationRequired,
    #[error("refused: kill switch is engaged; manual on-host reset required")]
    KillSwitchEngaged,
}

/// Outcome applied by the runner: the runner owns the hot path and decides
/// how to apply `PauseKind`/`ResumeKind` through the narrow channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOutcome {
    /// Human-readable status text (safe to post to Discord; no secrets).
    Message(String),
    Pause,
    Resume,
}

/// Audit record for the append-only command log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub user: String,
    pub role: Role,
    pub command: ControlCommand,
    pub result: String,
}

/// Gate + execute a command. `killed` reflects the bot's kill-switch state
/// (read through the narrow channel, never Discord-writable).
pub fn handle_command(
    cmd: ControlCommand,
    role: Role,
    user: &str,
    killed: bool,
    status_text: &str,
) -> Result<(ControlOutcome, AuditEntry), ControlError> {
    let authorized = matches!(role, Role::Operator | Role::Admin);
    let outcome = match &cmd {
        ControlCommand::Status | ControlCommand::Alerts => {
            ControlOutcome::Message(status_text.to_string())
        }
        ControlCommand::Pause { confirmed } => {
            if !authorized {
                return Err(ControlError::Unauthorized);
            }
            if !confirmed {
                return Err(ControlError::ConfirmationRequired);
            }
            ControlOutcome::Pause
        }
        ControlCommand::Resume { confirmed } => {
            if !authorized {
                return Err(ControlError::Unauthorized);
            }
            if killed {
                return Err(ControlError::KillSwitchEngaged);
            }
            if !confirmed {
                return Err(ControlError::ConfirmationRequired);
            }
            ControlOutcome::Resume
        }
    };
    let audit = AuditEntry {
        user: user.to_string(),
        role,
        command: cmd,
        result: format!("{outcome:?}"),
    };
    Ok((outcome, audit))
}

/// Discord transport allowlist: the ONLY commands the Discord bot process may
/// expose. `Resume` (unpause) is deliberately absent: resume is an on-host,
/// owner-signed action, never a chat command. Allowlist changes, limit
/// changes, sweeps, and ownership moves have no command variants at all, so
/// they cannot be wired to Discord by any means.
pub fn discord_exposed(cmd: &ControlCommand) -> bool {
    // Exhaustive match: adding a state-changing variant forces the author to
    // decide its Discord exposure here (fail closed by default).
    match cmd {
        ControlCommand::Status | ControlCommand::Alerts => true,
        ControlCommand::Pause { .. } => true,
        ControlCommand::Resume { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_gets_read_only() {
        let (out, audit) =
            handle_command(ControlCommand::Status, Role::Viewer, "alice", false, "ok").unwrap();
        assert!(matches!(out, ControlOutcome::Message(_)));
        assert_eq!(audit.user, "alice");
    }

    #[test]
    fn viewer_cannot_pause() {
        assert_eq!(
            handle_command(
                ControlCommand::Pause { confirmed: true },
                Role::Viewer,
                "bob",
                false,
                "ok"
            ),
            Err(ControlError::Unauthorized)
        );
    }

    #[test]
    fn operator_needs_confirmation() {
        assert_eq!(
            handle_command(
                ControlCommand::Pause { confirmed: false },
                Role::Operator,
                "op",
                false,
                "ok"
            ),
            Err(ControlError::ConfirmationRequired)
        );
        let (out, _) = handle_command(
            ControlCommand::Pause { confirmed: true },
            Role::Operator,
            "op",
            false,
            "ok",
        )
        .unwrap();
        assert_eq!(out, ControlOutcome::Pause);
    }

    #[test]
    fn resume_refused_while_killed() {
        assert_eq!(
            handle_command(
                ControlCommand::Resume { confirmed: true },
                Role::Admin,
                "adm",
                true,
                "ok"
            ),
            Err(ControlError::KillSwitchEngaged)
        );
    }

    #[test]
    fn discord_exposes_pause_and_readonly_only() {
        // Transport holds the pauser key only: Status/Alerts/Pause are
        // wired; Resume (unpause) is NEVER a Discord command.
        assert!(discord_exposed(&ControlCommand::Status));
        assert!(discord_exposed(&ControlCommand::Alerts));
        assert!(discord_exposed(&ControlCommand::Pause { confirmed: true }));
        assert!(!discord_exposed(&ControlCommand::Resume {
            confirmed: true
        }));
    }

    #[test]
    fn no_fund_moving_command_variants_exist() {
        // Exhaustive match over every command: there is no Sweep / Allowlist
        // / Ownership / Limit variant to wire, by construction. Adding one
        // breaks this compile-time gate until `discord_exposed` decides it.
        fn exposes(cmd: ControlCommand) -> bool {
            match cmd {
                ControlCommand::Status => true,
                ControlCommand::Alerts => true,
                ControlCommand::Pause { .. } => true,
                ControlCommand::Resume { .. } => false,
            }
        }
        assert!(exposes(ControlCommand::Status));
        assert!(!exposes(ControlCommand::Resume { confirmed: true }));
    }
}
