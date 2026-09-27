//! The deal a customer seals before this platform pays their payee, and the one refusal it waits
//! for before it does.
//!
//! The platform pays first: Grid sends the money — naira to a bank in Lagos, shillings to an M-PESA
//! wallet — funded just in time in USDB, Grid's dollar on Spark, from the platform's own treasury.
//! It is then reimbursed out of the customer's escrow, at the price in sats the customer agreed,
//! and only when the cosigner, with its own read-only Grid token, has fetched both records below
//! and found what the customer sealed.
//!
//! The customer agrees a price, not a cost. What the platform paid Grid is its own affair — the
//! USDB/BTC rate and its margin are inside the price — so the cosigner checks what the customer
//! cares about: the money arrived, at the account they typed, and no more than the price left.

use std::collections::BTreeMap;

use cosigner::evidence::{HttpGet, OnUnavailable, Predicate};
use cosigner::policy::Policy;

/// Grid's own origin: where the platform dials it unless told otherwise, and — spelled exactly as
/// the image's credential origin, which `credential()` compares as a string, so never with `:443`
/// — where the enclave reaches it.
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

/// Does the pre-flight's answer mean this platform will be repaid? The deal's deadline if so.
///
/// Three things, all from the cosigner and none from the app, which is the party a platform that
/// pays first is guarding against:
///
/// - the refusal is exactly [`PREFLIGHT_REFUSAL`]: every term holds but the payout completing;
/// - the escrow is committed to the policy this platform **offered** — `all_of` stops at its first
///   failing term, so a policy with a term appended after `status` refuses in exactly those words
///   now and for ever after;
/// - the deal has at least `min_left` seconds to run, since nothing is released after it ends.
pub fn preflight_clears(
    reason: &str,
    deal: Option<&cosigner::escrow_session::DealTerms>,
    offered: &Policy,
    now: i64,
    min_left: i64,
) -> Result<i64, String> {
    if reason != PREFLIGHT_REFUSAL {
        return Err(format!("the cosigner would not reimburse it — {reason}"));
    }
    let deal = deal.ok_or("the cosigner did not say which deal the escrow is committed to")?;
    if deal.policy_sha256 != cosigner::policy::policy_sha256(offered) {
        return Err("the escrow is committed to another policy than the one offered".into());
    }
    let left = deal.deadline - now;
    if left < min_left {
        return Err(format!(
            "the deal ends in {left}s, and a payout needs at least {min_left}s to be repaid in"
        ));
    }
    Ok(deal.deadline)
}

/// What the customer asked for, as the policy will hold it.
pub struct Deal<'a> {
    /// Where the platform is paid, as the script an output pays.
    pub platform_script_hex: &'a str,
    /// Grid as the ENCLAVE reaches it, which is not always where the platform dials it: a fake Grid
    /// on this host is `http://192.168.127.254:<port>` from inside the enclave. Spelled exactly as
    /// the image's `SERVICE_CREDENTIAL_ORIGIN_GRID`, or the cosigner sends its token nowhere.
    pub grid_origin: &'a str,
    /// Grid's id for the payee's account.
    pub account_id: &'a str,
    /// The account the customer typed, by Grid's names for its fields — `accountNumber` and
    /// `bankName`, or `phoneNumber` and `provider`. Each is checked against Grid's record of
    /// `account_id`, so an id that names somebody else's account is refused.
    pub payee: BTreeMap<String, String>,
    /// Chosen by the customer's app. Carried as the payout's `description`, so one payout can
    /// satisfy only the deal that named it — the cosigner's replay ledger is per wallet, and
    /// without this one payout could be claimed from two customers' escrows.
    pub deal_tag: &'a str,
    /// The payee's currency, and how much of it, in minor units, they must receive.
    pub currency: &'a str,
    pub amount_minor: i64,
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

/// The payee's account is the one the customer typed: every field of it.
///
/// A fixed path: the account is known when the deal is sealed. A Grid external account cannot be
/// edited in place (the API has no PATCH), so what this finds is what the payout was sent to.
fn the_payee(deal: &Deal) -> HttpGet {
    HttpGet {
        provider: deal.grid_origin.into(),
        path: format!("{GRID_API}/platform/external-accounts/{}", deal.account_id),
        credentials: CREDENTIAL_KEY.into(),
        expect: deal
            .payee
            .iter()
            .map(|(field, value)| Predicate::Equals {
                at: format!("accountInfo.{field}"),
                value: value.clone(),
            })
            .collect(),
        on_unavailable: OnUnavailable::Pending,
    }
}

/// The payout named by the release completed, for this deal, to that account, for at least the
/// amount agreed, in its currency.
fn the_payout(deal: &Deal) -> HttpGet {
    HttpGet {
        provider: deal.grid_origin.into(),
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
                value: deal.currency.into(),
            },
            Predicate::AtLeast {
                at: "receivedAmount.amount".into(),
                value: deal.amount_minor,
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

/// The policy judged by the cosigner's own evaluator: against Grid records recorded in the
/// sandbox — one naira payout's transaction straight after quoting and again once it completed,
/// and its payee — and, for every rail, against records the fake Grid makes.
#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use cosigner::evidence::{Evidence, ReleaseFacts};
    use cosigner::policy::{enforce_release, OutputView};
    use serde_json::Value;

    use super::*;
    use crate::corridors::{self, Corridor, Rail};
    use crate::fake_grid::{self, Config, FakeGrid};
    use crate::grid;

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
            grid_origin: GRID_ORIGIN,
            account_id: "ExternalAccount:01a0df4a-9a9d-7e0c-0000-2fdf81a58f46",
            payee: [("accountNumber", "0123456789"), ("bankName", "OPay")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            deal_tag: "spike-ok-c4164034163d8d94",
            currency: "NGN",
            amount_minor: 3_000_000,
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

    /// The pre-flight clears only the deal this platform offered, with time left to be repaid in —
    /// learned from the cosigner, never from the app.
    #[test]
    fn the_preflight_clears_only_the_offered_deal_with_time_to_run() {
        use cosigner::escrow_session::DealTerms;
        let offered = policy_for(&alices_deal());
        let now = 1_800_000_000;
        let sealed = |policy: &Policy, deadline: i64| DealTerms {
            opened_at: now - 10,
            deadline,
            policy_sha256: cosigner::policy::policy_sha256(policy),
        };
        let hour = now + 3_600;

        assert_eq!(
            preflight_clears(PREFLIGHT_REFUSAL, Some(&sealed(&offered, hour)), &offered, now, 300),
            Ok(hour)
        );
        // A term appended after `status` refuses in the same words now, and for ever after.
        let appended = Policy::AllOf {
            of: vec![offered.clone(), Policy::Never],
        };
        let refused =
            preflight_clears(PREFLIGHT_REFUSAL, Some(&sealed(&appended, hour)), &offered, now, 300);
        assert!(refused.unwrap_err().contains("another policy"));
        // A deal too short to be repaid in.
        let refused =
            preflight_clears(PREFLIGHT_REFUSAL, Some(&sealed(&offered, now + 60)), &offered, now, 300);
        assert!(refused.unwrap_err().contains("ends in 60s"));
        // No terms at all: an older cosigner, or an answer that is not about this escrow's deal.
        assert!(preflight_clears(PREFLIGHT_REFUSAL, None, &offered, now, 300).is_err());
        // Any other refusal is a no.
        let refused = preflight_clears(
            "this escrow is not committed to a deal, so there is nothing to release",
            Some(&sealed(&offered, hour)),
            &offered,
            now,
            300,
        );
        assert!(refused.unwrap_err().contains("would not reimburse"));
    }

    #[test]
    fn the_price_rounds_up_to_a_whole_sat() {
        assert_eq!(price_sats(23_005_382, 1_000), Some(PRICE));
        assert_eq!(price_sats(23_000_000, 1_000), Some(23_000));
        assert_eq!(price_sats(0, 1_000), None, "a cap of nothing would deny every release");
    }

    /// One payout on the fake Grid, fetched as the cosigner would: the payee, and the transaction
    /// before funding and after it completed. SYNTHETIC — the fake's records, not Grid's.
    struct Synthetic {
        account_id: String,
        payee: BTreeMap<String, String>,
        amount: i64,
        account: Value,
        pending: Value,
        completed: Value,
    }

    const TAG: &str = "deal-0c4e8d21a9f3b7e6";
    const COMPLETE_AFTER: u64 = 5;

    fn synthetic(corridor: &'static Corridor, rail: &Rail) -> Synthetic {
        let fake = FakeGrid::manual(Config {
            transact: "platform:transact".into(),
            view: "enclave:view".into(),
            complete_after_secs: COMPLETE_AFTER,
            quote_ttl_secs: 180,
        });
        let payee = fake_grid::example_fields(corridor, rail, "789");
        let body = grid::account_body(corridor.currency, rail.account_type, &payee, "Ada Obi");
        let account = fake.create_account(None, &body).1;
        let account_id = account["id"].as_str().unwrap().to_string();
        let amount = rail.min_minor;
        let quote = fake.create_quote(None, &grid::quote_body(&account_id, amount, TAG)).1;
        let transaction_id = quote["transactionId"].as_str().unwrap();
        let pending = fake.transaction(transaction_id).1;
        fake.fund(None, &grid::fund_body(quote["id"].as_str().unwrap()));
        fake.advance(COMPLETE_AFTER);
        let completed = fake.transaction(transaction_id).1;
        for record in [&account, &pending, &completed] {
            assert_eq!(record["simulated"], true, "labelled as the fake's");
        }
        Synthetic { account_id, payee, amount, account, pending, completed }
    }

    fn deal_on<'a>(corridor: &'a Corridor, s: &'a Synthetic) -> Deal<'a> {
        Deal {
            platform_script_hex: PLATFORM_SCRIPT,
            grid_origin: "http://192.168.127.254:7300",
            account_id: &s.account_id,
            payee: s.payee.clone(),
            deal_tag: TAG,
            currency: corridor.currency,
            amount_minor: s.amount,
            price_sats: PRICE,
        }
    }

    /// Every rail: a completed payout releases the price, and before funding the only refusal is
    /// the pre-flight's, word for word.
    #[test]
    fn on_every_rail_the_preflight_holds_and_a_completed_payout_releases() {
        for (corridor, rail) in corridors::rails() {
            let s = synthetic(corridor, rail);
            let deal = deal_on(corridor, &s);
            let on = format!("{} {}", corridor.country, rail.rail);
            assert_eq!(
                judge(&deal, PRICE, s.pending.clone(), s.account.clone()),
                Err(PREFLIGHT_REFUSAL.to_string()),
                "{on}"
            );
            assert_eq!(judge(&deal, PRICE, s.completed.clone(), s.account.clone()), Ok(()), "{on}");
        }
    }

    /// Every rail: another payee, another currency, less money or another deal is refused.
    #[test]
    fn on_every_rail_anything_but_what_was_sealed_is_refused() {
        for (corridor, rail) in corridors::rails() {
            let s = synthetic(corridor, rail);
            let deal = deal_on(corridor, &s);
            let refused = |tx: Value, payee: Value, deal: &Deal| {
                judge(deal, PRICE, tx, payee).expect_err("refused")
            };
            for field in s.payee.keys() {
                let mut elsewhere = s.account.clone();
                elsewhere["accountInfo"][field] = "somebody else's".into();
                let why = refused(s.completed.clone(), elsewhere, &deal);
                assert!(why.starts_with(&format!("accountInfo.{field} is ")), "{why}");
            }
            let mut dollars = s.completed.clone();
            dollars["receivedAmount"]["currency"]["code"] = "USD".into();
            let why = refused(dollars, s.account.clone(), &deal);
            assert!(why.starts_with("receivedAmount.currency.code is "), "{why}");
            let mut short = s.completed.clone();
            short["receivedAmount"]["amount"] = (s.amount - 1).into();
            let why = refused(short, s.account.clone(), &deal);
            assert!(why.starts_with("receivedAmount.amount is "), "{why}");
            let bobs = Deal { deal_tag: "bob-0c4e8d21a9f3b7e6", ..deal_on(corridor, &s) };
            let why = refused(s.completed.clone(), s.account.clone(), &bobs);
            assert!(why.starts_with("description is "), "{why}");
        }
    }
}
