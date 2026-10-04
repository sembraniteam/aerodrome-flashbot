//! L4 executor roles and caps: on-chain read-back vs the profile.
//!
//! The live binary reads `owner`/`operator`/`pauser`/`maxFlashUSDC` from
//! chain at startup and compares them with the profile; any mismatch refuses
//! (fail closed). The off-chain `hard_max` must sit at or below the on-chain
//! limit. Reads are `eth_call` only -- no signing, no broadcast.

use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::sol;
use std::future::IntoFuture as _;
use std::time::Duration;
use thiserror::Error;

sol! {
    /// Read-only role getters of `FlashArbExecutor` (public state vars).
    #[sol(rpc)]
    interface IExecutorRoles {
        function owner() external view returns (address);
        function operator() external view returns (address);
        function pauser() external view returns (address);
        function maxFlashUSDC() external view returns (uint256);
    }
}

/// On-chain role matrix (observed via read-back, or expected from profile).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleMatrix {
    pub owner: Address,
    pub operator: Address,
    pub pauser: Address,
    pub max_flash_usdc: u64,
}

impl RoleMatrix {
    /// Expected matrix from the profile's `[live]` section.
    pub fn from_profile(profile: &super::profile::LiveProfile) -> Self {
        let l = &profile.live_section;
        Self {
            owner: l.owner,
            operator: l.operator,
            pauser: l.pauser,
            max_flash_usdc: l.onchain_max_flash_usdc,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RoleError {
    #[error("owner mismatch (profile != chain)")]
    OwnerMismatch,
    #[error("operator mismatch (profile != chain)")]
    OperatorMismatch,
    #[error("pauser mismatch (profile != chain)")]
    PauserMismatch,
    #[error("maxFlashUSDC mismatch: profile {0} != chain {1}")]
    MaxFlashMismatch(u64, u64),
    #[error("off-chain hard_max {0} exceeds on-chain maxFlashUSDC {1}")]
    HardAboveOnchain(u64, u64),
    #[error("role read-back failed: {0}")]
    ReadFailed(String),
}

/// Compare the profile matrix with the observed one. Any mismatch refuses.
/// `offchain_hard_max` is the profile's `hard_max_flash_usdc`: it must sit
/// at or below the on-chain limit.
pub fn verify_roles(
    expected: &RoleMatrix,
    observed: &RoleMatrix,
    offchain_hard_max: u64,
) -> Result<(), RoleError> {
    if expected.owner != observed.owner {
        return Err(RoleError::OwnerMismatch);
    }
    if expected.operator != observed.operator {
        return Err(RoleError::OperatorMismatch);
    }
    if expected.pauser != observed.pauser {
        return Err(RoleError::PauserMismatch);
    }
    if expected.max_flash_usdc != observed.max_flash_usdc {
        return Err(RoleError::MaxFlashMismatch(
            expected.max_flash_usdc,
            observed.max_flash_usdc,
        ));
    }
    if offchain_hard_max > observed.max_flash_usdc {
        return Err(RoleError::HardAboveOnchain(
            offchain_hard_max,
            observed.max_flash_usdc,
        ));
    }
    Ok(())
}

/// Read-only role read-back (`eth_call`s against the executor). `Err` =
/// RPC problem (fail closed downstream, never default to "match").
pub async fn read_roles<P>(provider: &P, executor: Address) -> Result<RoleMatrix, RoleError>
where
    P: Provider + Clone,
{
    let contract = IExecutorRoles::new(executor, provider.clone());
    let call = async {
        let owner = contract
            .owner()
            .call()
            .into_future()
            .await
            .map_err(|e| e.to_string())?;
        let operator = contract
            .operator()
            .call()
            .into_future()
            .await
            .map_err(|e| e.to_string())?;
        let pauser = contract
            .pauser()
            .call()
            .into_future()
            .await
            .map_err(|e| e.to_string())?;
        let max_flash = contract
            .maxFlashUSDC()
            .call()
            .into_future()
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(RoleMatrix {
            owner,
            operator,
            pauser,
            max_flash_usdc: u64::try_from(max_flash).unwrap_or(u64::MAX),
        })
    };
    tokio::time::timeout(Duration::from_secs(15), call)
        .await
        .map_err(|_| RoleError::ReadFailed("role read-back timed out".to_string()))?
        .map_err(RoleError::ReadFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix(owner_byte: u8) -> RoleMatrix {
        RoleMatrix {
            owner: Address::repeat_byte(owner_byte),
            operator: Address::repeat_byte(0x22),
            pauser: Address::repeat_byte(0x33),
            max_flash_usdc: 5_000_000_000,
        }
    }

    #[test]
    fn accepts_matching_matrix() {
        let m = matrix(0x11);
        assert!(verify_roles(&m, &m, 50_000_000).is_ok());
    }

    #[test]
    fn refuses_each_role_mismatch() {
        // L10: role-matrix mismatch refuses startup (every field).
        let expected = matrix(0x11);
        let mut observed = matrix(0x99);
        assert_eq!(
            verify_roles(&expected, &observed, 50_000_000),
            Err(RoleError::OwnerMismatch)
        );
        observed = matrix(0x11);
        observed.operator = Address::repeat_byte(0x44);
        assert_eq!(
            verify_roles(&expected, &observed, 50_000_000),
            Err(RoleError::OperatorMismatch)
        );
        observed = matrix(0x11);
        observed.pauser = Address::repeat_byte(0x44);
        assert_eq!(
            verify_roles(&expected, &observed, 50_000_000),
            Err(RoleError::PauserMismatch)
        );
        observed = matrix(0x11);
        observed.max_flash_usdc = 1;
        assert_eq!(
            verify_roles(&expected, &observed, 50_000_000),
            Err(RoleError::MaxFlashMismatch(5_000_000_000, 1))
        );
    }

    #[test]
    fn refuses_hard_max_above_onchain_limit() {
        let m = matrix(0x11);
        assert_eq!(
            verify_roles(&m, &m, 5_000_000_001),
            Err(RoleError::HardAboveOnchain(5_000_000_001, 5_000_000_000))
        );
    }
}
