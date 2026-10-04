//! Operator float cap: the operator balance is gas only.
//!
//! Checked at startup, before every submit (L5 pipeline gate), and in L6
//! reconciliation. The USD-equivalent conversion is fail-closed: an unknown
//! or stale balance (`None`) refuses. Over-cap refuses.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FloatCapError {
    #[error("operator balance unknown or stale: refuse (fail closed)")]
    UnknownBalance,
    #[error("operator float {0} exceeds cap {1} (USDC base units equiv)")]
    OverCap(u64, u64),
}

/// `balance_equiv` is the operator balance in USD-equivalent base units
/// (6 decimals); `None` means the fetch failed or the price is stale.
pub fn check_float_cap(balance_equiv: Option<u64>, cap: u64) -> Result<(), FloatCapError> {
    match balance_equiv {
        None => Err(FloatCapError::UnknownBalance),
        Some(b) if b > cap => Err(FloatCapError::OverCap(b, cap)),
        Some(_) => Ok(()),
    }
}

/// Convert a native balance (wei, 18dp) to USD-equivalent base units (6dp)
/// at an operator-attested spot price (`price_cents_per_eth`, e.g. 300_000
/// = $3000). Integer math, saturating: `wei * cents / 1e14`.
///
/// The price MUST carry a staleness bound at the call site (the live binary
/// refuses prices older than 300 s); this function converts, it does not
/// attest freshness.
pub fn eth_wei_to_usdc_base(wei: u128, price_cents_per_eth: u64) -> u64 {
    const WEI_PER_CENT_BASE: u128 = 100_000_000_000_000; // 1e14
    wei.saturating_mul(price_cents_per_eth as u128)
        .saturating_div(WEI_PER_CENT_BASE)
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_balance_within_cap() {
        assert!(check_float_cap(Some(0), 20_000_000).is_ok());
        assert!(check_float_cap(Some(20_000_000), 20_000_000).is_ok());
    }

    #[test]
    fn refuses_over_cap_and_unknown() {
        // L10: operator over float cap refuses (startup + pre-submit).
        assert_eq!(
            check_float_cap(Some(20_000_001), 20_000_000),
            Err(FloatCapError::OverCap(20_000_001, 20_000_000))
        );
        assert_eq!(
            check_float_cap(None, 20_000_000),
            Err(FloatCapError::UnknownBalance)
        );
    }

    #[test]
    fn wei_to_usdc_base_math() {
        // 1 ETH @ $3000 = $3000 = 3_000_000_000 base units.
        assert_eq!(
            eth_wei_to_usdc_base(1_000_000_000_000_000_000, 300_000),
            3_000_000_000
        );
        assert_eq!(eth_wei_to_usdc_base(0, 300_000), 0);
        // Dust: 0.001 ETH @ $3000 = $3.
        assert_eq!(
            eth_wei_to_usdc_base(1_000_000_000_000_000, 300_000),
            3_000_000
        );
        // Saturates instead of wrapping on absurd inputs.
        assert_eq!(eth_wei_to_usdc_base(u128::MAX, u64::MAX), u64::MAX);
    }
}
