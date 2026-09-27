//! The deal a customer seals before this platform pays their bank, and the one refusal it waits
//! for before it does.
//!
//! The platform pays first: Grid sends the naira, funded just in time in USDB — Grid's dollar, on
//! Spark — from the platform's own treasury. It is then reimbursed out of the customer's escrow, at
//! the price in sats the customer agreed, and only when the cosigner, with its own read-only Grid
//! token, has fetched both records below and found what the customer sealed.
//!
//! The customer agrees a price, not a cost. What the platform paid Grid is its own affair — the
//! USDB/BTC rate and its margin are inside the price — so the cosigner checks what the customer
//! cares about: the naira arrived, at the account they typed, and no more than the price left.

use cosigner::evidence::{HttpGet, OnUnavailable, Predicate};
use cosigner::policy::Policy;

/// Grid, as the cosigner reaches it. Spelled exactly as the image's credential origin:
/// `credential()` compares the two as strings, so `https://api.lightspark.com:443` would not match.
pub const GRID_ORIGIN: &str = "https://api.lightspark.com";
/// The Grid API version this was written against. It is part of every path.
pub const GRID_API: &str = "/grid/2025-10-13";
/// The credential the enclave's image carries for Grid — a VIEW-only token, named, never valued.
pub const CREDENTIAL_KEY: &str = "GRID";

/// The one refusal that clears this platform to pay.
///
/// Asked before funding, a release against the payout's transaction can only fail on the LAST
/// predicate — the payout has not completed, because it has not been paid for. Anything else means
/// something this platform would not be reimbursed for: no deal sealed, an empty escrow, a policy
/// that names another account or amount. So it pays on exactly this text and nothing else. The
/// cosigner passes a predicate's refusal through unchanged; a test pins this to its wording.
///
/// ponytail: matching refusal text. Upgrade path: a typed refusal code on `release-refused`.
pub const PREFLIGHT_REFUSAL: &str = r#"status is "PENDING", not "COMPLETED""#;

/// What the customer asked for, as the policy will hold it.
pub struct Deal<'a> {
    /// Where the platform is paid, as the script an output pays.
    pub platform_script_hex: &'a str,
    /// Grid's id for the payee's bank account.
    pub account_id: &'a str,
    /// The account number and bank the customer typed. Checked against Grid's record of
    /// `account_id`, so an id that names somebody else's account is refused.
    pub account_number: &'a str,
    pub bank_name: &'a str,
    /// Chosen by the customer's app. Carried as the payout's `description`, so one payout can
    /// satisfy only the deal that named it — the cosigner's replay ledger is per wallet, and
    /// without this one payout could be claimed from two customers' escrows.
    pub deal_tag: &'a str,
    /// Naira, in kobo, the payee must receive.
    pub amount_kobo: i64,
    /// The price the customer agreed, in sats — the most the escrow may release for this payout.
    pub price_sats: u64,
}

/// USDB's smallest unit: it has six decimals.
const MICRO_USDB_PER_USD: u128 = 1_000_000;

/// The platform's price in sats for a payout Grid quoted at `micro_usdb`.
///
/// Rounded up to a whole sat: this is the platform's quote, shown to the customer before they
/// agree to it. `None` for nothing, and for an amount too large to be a price.
pub fn price_sats(micro_usdb: u64, sats_per_usd: u64) -> Option<u64> {
    let sats = (u128::from(micro_usdb) * u128::from(sats_per_usd)).div_ceil(MICRO_USDB_PER_USD);
    u64::try_from(sats).ok().filter(|&s| s > 0)
}

/// The sealed policy for one payout.
///
/// Cheap checks first and the payout's `status` last, so that before funding the only thing that
/// can fail is the status — see [`PREFLIGHT_REFUSAL`].
pub fn policy_for(deal: &Deal) -> Policy {
    // Exactly the agreed price. A re-quote at another USDB cost does not change it: that cost is
    // the platform's.
    let cap = deal.price_sats;
    Policy::AllOf {
        of: vec![
            Policy::OutputsOnlyTo {
                scripts: vec![deal.platform_script_hex.to_ascii_lowercase()],
            },
            Policy::TotalOutMax { sats: cap },
            Policy::ReleasedTotalMax { sats: cap },
            Policy::FeeMax { sats: 0 },
            Policy::HttpGet(Box::new(the_payee(deal))),
            Policy::HttpGet(Box::new(the_payout(deal))),
        ],
    }
}

/// The payee's account is the one the customer typed.
///
/// A fixed path: the account is known when the deal is sealed. A Grid external account cannot be
/// edited in place (the API has no PATCH), so what this finds is what the payout was sent to.
fn the_payee(deal: &Deal) -> HttpGet {
    HttpGet {
        provider: GRID_ORIGIN.into(),
        path: format!("{GRID_API}/platform/external-accounts/{}", deal.account_id),
        credentials: CREDENTIAL_KEY.into(),
        expect: vec![
            Predicate::Equals {
                at: "accountInfo.accountNumber".into(),
                value: deal.account_number.into(),
            },
            Predicate::Equals {
                at: "accountInfo.bankName".into(),
                value: deal.bank_name.into(),
            },
        ],
        on_unavailable: OnUnavailable::Pending,
    }
}

/// The payout named by the release completed, for this deal, to that account, for at least the
/// naira agreed.
fn the_payout(deal: &Deal) -> HttpGet {
    HttpGet {
        provider: GRID_ORIGIN.into(),
        path: format!("{GRID_API}/transactions/{{reference}}"),
        credentials: CREDENTIAL_KEY.into(),
        expect: vec![
            Predicate::MatchesReference { at: "id".into() },
            Predicate::Equals {
                at: "type".into(),
                value: "OUTGOING".into(),
            },
            Predicate::Equals {
                at: "description".into(),
                value: deal.deal_tag.into(),
            },
            Predicate::Equals {
                at: "destination.accountId".into(),
                value: deal.account_id.into(),
            },
            Predicate::Equals {
                at: "receivedAmount.currency.code".into(),
                value: "NGN".into(),
            },
            Predicate::AtLeast {
                at: "receivedAmount.amount".into(),
                value: deal.amount_kobo,
            },
            // Last. See `PREFLIGHT_REFUSAL`.
            Predicate::Equals {
                at: "status".into(),
                value: "COMPLETED".into(),
            },
        ],
        on_unavailable: OnUnavailable::Pending,
    }
}

/// The policy judged by the cosigner's own evaluator, against Grid records recorded in the sandbox:
/// one payout's transaction straight after quoting and again once it completed, and its payee.
#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use cosigner::evidence::{Evidence, ReleaseFacts};
    use cosigner::policy::{enforce_release, OutputView};
    use serde_json::Value;

    use super::*;

    const PLATFORM_SCRIPT: &str =
        "51204444444444444444444444444444444444444444444444444444444444444444";
    /// 23.005382 USDB at 1,000 sats a dollar, rounded up.
    const PRICE: u64 = 23_006;

    fn fixture(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    fn completed() -> Value {
        fixture(include_str!("../tests/fixtures/transaction.json"))
    }

    fn pending() -> Value {
        fixture(include_str!("../tests/fixtures/transaction_pending.json"))
    }

    fn account() -> Value {
        fixture(include_str!("../tests/fixtures/external_account.json"))
    }

    /// What Alice sealed. It matches the recordings, as a real deal matches its payout.
    fn alices_deal() -> Deal<'static> {
        Deal {
            platform_script_hex: PLATFORM_SCRIPT,
            account_id: "ExternalAccount:01a0df4a-9a9d-7e0c-0000-2fdf81a58f46",
            account_number: "0123456789",
            bank_name: "OPay",
            deal_tag: "spike-ok-c4164034163d8d94",
            amount_kobo: 3_000_000,
            price_sats: PRICE,
        }
    }

    /// A release of `sats` against the transaction's id, with Grid answering `tx` and `payee`.
    fn judge(deal: &Deal, sats: u64, tx: Value, payee: Value) -> Result<(), String> {
        let policy = policy_for(deal);
        policy.validate()?;
        let facts = ReleaseFacts {
            reference: tx["id"].as_str().unwrap().into(),
            sats,
            fee_sats: 0,
            already_released_sats: 0,
        };
        let evidence: BTreeMap<String, Evidence> = policy
            .evidence_needed(&facts)
            .into_iter()
            .map(|request| {
                let body = if request.path.contains("/external-accounts/") {
                    payee.clone()
                } else {
                    tx.clone()
                };
                (request.key(), Evidence::Json(body))
            })
            .collect();
        let outputs = [OutputView {
            script_pubkey_hex: PLATFORM_SCRIPT.into(),
            sats,
        }];
        enforce_release(&policy, Some(&outputs[..]), &BTreeSet::new(), &facts, &evidence)
    }

    #[test]
    fn a_completed_payout_releases_the_agreed_price() {
        assert_eq!(judge(&alices_deal(), PRICE, completed(), account()), Ok(()));
    }

    /// Straight after quoting, every term but the status already holds — and the text the platform
    /// funds on is the cosigner's own wording, word for word.
    #[test]
    fn before_funding_the_only_refusal_is_the_preflights() {
        assert_eq!(
            judge(&alices_deal(), PRICE, pending(), account()),
            Err(PREFLIGHT_REFUSAL.to_string())
        );
    }

    #[test]
    fn a_failed_payout_is_refused() {
        let mut failed = completed();
        failed["status"] = "FAILED".into();
        assert_eq!(
            judge(&alices_deal(), PRICE, failed, account()),
            Err(r#"status is "FAILED", not "COMPLETED""#.to_string())
        );
    }

    /// The same completed payout cannot pay for a second customer's deal.
    #[test]
    fn alices_payout_does_not_pay_for_bobs_deal() {
        let bobs = Deal {
            deal_tag: "bob-0c4e8d21a9f3b7e6",
            ..alices_deal()
        };
        let refused = judge(&bobs, PRICE, completed(), account()).unwrap_err();
        assert!(refused.starts_with("description is "), "{refused}");
    }

    /// An account id whose record holds somebody else's bank details is refused.
    #[test]
    fn a_payee_with_other_bank_details_is_refused() {
        let mut elsewhere = account();
        elsewhere["accountInfo"]["accountNumber"] = "9999999999".into();
        let refused = judge(&alices_deal(), PRICE, completed(), elsewhere).unwrap_err();
        assert!(refused.starts_with("accountInfo.accountNumber is "), "{refused}");
    }

    #[test]
    fn a_release_above_the_agreed_price_is_refused() {
        assert_eq!(
            judge(&alices_deal(), PRICE + 1, completed(), account()),
            Err(format!("{} sats leaves the wallet, over the {PRICE} sat cap", PRICE + 1))
        );
    }

    #[test]
    fn short_naira_is_refused() {
        let mut short = completed();
        short["receivedAmount"]["amount"] = 2_999_999.into();
        let refused = judge(&alices_deal(), PRICE, short, account()).unwrap_err();
        assert!(refused.starts_with("receivedAmount.amount is "), "{refused}");
    }

    #[test]
    fn the_price_rounds_up_to_a_whole_sat() {
        assert_eq!(price_sats(23_005_382, 1_000), Some(PRICE));
        assert_eq!(price_sats(23_000_000, 1_000), Some(23_000));
        assert_eq!(price_sats(0, 1_000), None, "a cap of nothing would deny every release");
    }
}
