//! A fake Lightspark Grid: the part of Grid's API this platform and its cosigner use, answering in
//! Grid's shapes (`tests/fixtures/` holds the recordings they are checked against) and moving no
//! money.
//!
//! For working without Grid's sandbox, which funds only some corridors. It keeps the sandbox's
//! convention: the last three digits of the payee's account number — its phone number, on mobile
//! money — decide what happens.
//!
//! | ending | when | what |
//! |---|---|---|
//! | 102 | the account is registered | the name check says `NOT_MATCHED` |
//! | 103 | | `PARTIAL_MATCH`, with a different name |
//! | 104 | | `PENDING` |
//! | 105 | | refused: 400 |
//! | 106 | | `UNSUPPORTED` |
//! | 107 | | `CHECKED_BY_RECEIVING_FI` |
//! | 002 | the quote is funded | `FAILED`: `QUOTE_EXECUTION_FAILED`, and refunded |
//! | 003 | | `COMPLETED`, slowly: ten times `--complete-after-secs` |
//! | 005 | | `COMPLETED`, then `FAILED` when the bank returns it: `PAYOUT_RETURNED` |
//! | any other | | `COMPLETED` after `--complete-after-secs`, having been `PROCESSING` |
//!
//! A quote not funded within `--quote-ttl-secs` is `EXPIRED`, and a quote's status follows its
//! transaction's, as on Grid.
//!
//! Everything is under `/grid/2025-10-13` — a sealed policy names Grid's origin, never a path — and
//! needs HTTP Basic: the TRANSACT token for anything, the VIEW token only to read. Every write
//! honours `Idempotency-Key`. Every record says `"simulated": true`, and every reply carries
//! `x-simulated-payments: true`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::corridors::{self, Corridor, Rail};
use crate::payout::GRID_API;

/// The header every reply carries, so nobody has to read the body to know what this is.
pub const SIMULATED_HEADER: &str = "x-simulated-payments";

/// Where a manual clock starts: 2026-09-27T00:00:00Z.
pub const EPOCH: u64 = 1_790_467_200;

/// What Grid charges the platform for a payout, over the exchange: fifty cents of USDB.
const FEE_MICRO_USDB: u64 = 500_000;

/// The platform's Grid customer, on every transaction.
const CUSTOMER: &str = "00000000-0000-4000-8000-00000000c0de";

/// Banks the fake takes as a `bankName` and lists at `GET /discoveries`, a few per country and
/// spelled as Grid's sandbox and docs spell them. The networks a corridor's rails offer by name —
/// Ghana's mobile money, M-PESA — are listed too, after them, as on Grid.
const BANKS: &[(&str, &[&str])] = &[
    ("NG", &[
        "OPay", "PalmPay", "Moniepoint Microfinance Bank", "Kuda Microfinance Bank", "Access Bank",
        "GTBank Plc", "Guaranty Trust Bank", "Zenith Bank", "First Bank of Nigeria",
        "United Bank for Africa", "Wema Bank",
    ]),
    ("GH", &[
        "Gcb Bank Ltd", "Ecobank Ghana", "Absa Bank Ghana", "Stanbic Bank Ghana",
        "Fidelity Bank Ghana",
    ]),
    ("ZA", &[
        "Standard Bank (South Africa)", "Absa Bank", "First National Bank", "Nedbank",
        "Capitec Bank",
    ]),
];

/// Who may call, and how fast things happen.
#[derive(Debug, Clone)]
pub struct Config {
    /// `id:secret` of the token that may register, quote and fund: the platform's.
    pub transact: String,
    /// `id:secret` of the token that may only read: the enclave's.
    pub view: String,
    /// How long a funded payout takes.
    pub complete_after_secs: u64,
    /// How long a quote waits to be funded.
    pub quote_ttl_secs: u64,
}

/// A status and the reply's body.
pub type Reply = (StatusCode, Value);

pub struct FakeGrid {
    config: Config,
    /// `None`: wall time. Otherwise a clock that only [`FakeGrid::advance`] moves.
    manual_clock: Option<AtomicU64>,
    /// `None`: random UUIDs. Otherwise ids counted from 1.
    counted_ids: Option<AtomicU64>,
    inner: Mutex<Inner>,
}

// ponytail: in memory — a restart strands a payout in flight (dev only)
#[derive(Default)]
struct Inner {
    accounts: BTreeMap<String, Value>,
    /// By quote id.
    payouts: BTreeMap<String, Payout>,
    /// Transaction id to quote id.
    transactions: BTreeMap<String, String>,
    /// The first reply to each route's `Idempotency-Key`.
    replies: BTreeMap<String, Reply>,
}

/// A quote and the transaction made with it. Both statuses are worked out from the clock when read.
struct Payout {
    quote: Value,
    transaction: Value,
    /// The payee's last three digits: what funding leads to.
    ending: String,
    refund: String,
    created: u64,
    expires: u64,
    funded: Option<u64>,
}

enum Token {
    Transact,
    View,
}

impl FakeGrid {
    /// Wall time and random ids, as the binary serves. Random, because the cosigner's ledger
    /// remembers every reference it has repaid against: a restarted fake handing out
    /// `Transaction:…0001` again would have its payouts refused.
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            manual_clock: None,
            counted_ids: None,
            inner: Mutex::default(),
        })
    }

    /// A clock that stands at [`EPOCH`] until [`FakeGrid::advance`] moves it, and ids counted from
    /// 1: for tests.
    pub fn manual(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            manual_clock: Some(AtomicU64::new(EPOCH)),
            counted_ids: Some(AtomicU64::new(0)),
            inner: Mutex::default(),
        })
    }

    pub fn advance(&self, secs: u64) {
        let clock = self.manual_clock.as_ref().expect("only a manual clock is moved by hand");
        clock.fetch_add(secs, Ordering::SeqCst);
    }

    fn now(&self) -> u64 {
        match &self.manual_clock {
            Some(clock) => clock.load(Ordering::SeqCst),
            None => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }

    fn uuid(&self) -> String {
        if let Some(count) = &self.counted_ids {
            let n = count.fetch_add(1, Ordering::SeqCst) + 1;
            return format!("00000000-0000-4000-8000-{n:012x}");
        }
        let mut b: [u8; 16] = rand::random();
        b[6] = (b[6] & 0x0f) | 0x40; // version 4
        b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
        let h = hex::encode(b);
        format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
    }

    /// `POST /platform/external-accounts`: the payee, checked as Grid checks it, named by its
    /// ending.
    pub fn create_account(&self, key: Option<&str>, body: &Value) -> Reply {
        self.once("external-accounts", key, |state| {
            let currency = body["currency"].as_str().unwrap_or_default();
            let info = &body["accountInfo"];
            let account_type = info["accountType"].as_str().unwrap_or_default();
            // The corridor's fields: everything in `accountInfo` but its type and the payee.
            let fields: BTreeMap<String, String> = info
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(k, _)| !matches!(k.as_str(), "accountType" | "beneficiary"))
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect();
            // Ghana's two rails share an account type and differ in their fields.
            let Some((corridor, rail)) = corridors::rails().find(|(c, r)| {
                c.currency == currency
                    && r.account_type == account_type
                    && r.fields.iter().all(|f| fields.contains_key(f.key))
            }) else {
                return invalid(format!("no {currency} {account_type} takes these fields"));
            };
            if let Err(why) = rail.check_fields(&fields) {
                return invalid(why);
            }
            let full_name = info["beneficiary"]["fullName"].as_str().unwrap_or_default();
            let individual = info["beneficiary"]["beneficiaryType"] == "INDIVIDUAL";
            if !individual || full_name.trim().is_empty() || full_name.chars().count() > 250 {
                return invalid("beneficiary: an INDIVIDUAL, with a fullName".into());
            }
            if let Some(bank) = fields.get("bankName") {
                if !names(corridor).any(|name| name == bank.as_str()) {
                    return invalid(format!("bankName {bank:?} is not a name /discoveries lists"));
                }
            }
            let (check, name_at_bank) = match ending(info).as_str() {
                "102" => ("NOT_MATCHED", None),
                "103" => ("PARTIAL_MATCH", Some(format!("{full_name} (simulated partial match)"))),
                "104" => ("PENDING", None),
                "105" => return invalid("the account could not be checked (simulated)".into()),
                "106" => ("UNSUPPORTED", None),
                "107" => ("CHECKED_BY_RECEIVING_FI", None),
                _ => ("MATCHED", Some(full_name.to_string())),
            };
            let mut account_info = info.clone();
            account_info["paymentRails"] = json!([payment_rail(rail)]);
            let id = format!("ExternalAccount:{}", self.uuid());
            let mut account = json!({
                "accountInfo": account_info,
                "beneficiaryVerificationStatus": check,
                "currency": currency,
                "defaultUmaDepositAccount": false,
                "id": id,
                "status": "ACTIVE",
                "simulated": true,
            });
            if let Some(name) = name_at_bank {
                account["beneficiaryVerifiedData"] = json!({ "fullName": name });
            }
            state.accounts.insert(id, account.clone());
            (StatusCode::CREATED, account)
        })
    }

    /// `POST /quotes`: the quote, and the transaction it will be — `PENDING`, and already all the
    /// cosigner checks but its status.
    pub fn create_quote(&self, key: Option<&str>, body: &Value) -> Reply {
        self.once("quotes", key, |state| {
            let account_id = body["destination"]["accountId"].as_str().unwrap_or_default();
            let Some(account) = state.accounts.get(account_id) else {
                return invalid(format!("destination.accountId {account_id:?} is no account here"));
            };
            let funded_just_in_time = body["source"]["sourceType"] == "REALTIME_FUNDING"
                && body["source"]["currency"] == "USDB"
                && body["lockedCurrencySide"] == "RECEIVING";
            if !funded_just_in_time {
                return invalid("the fake quotes a set amount received, funded in USDB".into());
            }
            let Some(amount) = body["lockedCurrencyAmount"].as_u64().filter(|a| *a > 0) else {
                return invalid("lockedCurrencyAmount: whole minor units, more than none".into());
            };
            let currency = account["currency"].as_str().unwrap_or_default();
            let rate = micro_usdb_per_unit(currency);
            let per_unit = 10u128.pow(decimals(currency));
            let exchanged = (u128::from(amount) * u128::from(rate)).div_ceil(per_unit);
            let cost = u64::try_from(exchanged).unwrap_or(u64::MAX).saturating_add(FEE_MICRO_USDB);
            let now = self.now();
            let expires = now + self.config.quote_ttl_secs;
            let quote_id = format!("Quote:{}", self.uuid());
            let transaction_id = format!("Transaction:{}", self.uuid());
            let instructions = json!([{ "accountOrWalletInfo": {
                "accountType": "SPARK_WALLET",
                "address": "sparkrt1simulated",
                "assetType": "USDB",
                "invoice": "sparkrt1simulatedinvoice",
            }}]);
            let destination = json!({ "accountId": account_id, "destinationType": "ACCOUNT" });
            let quote = json!({
                "counterpartyInformation": {
                    "FI_LEGAL_ENTITY_NAME": "Simulated FI",
                    "FULL_NAME": account["accountInfo"]["beneficiary"]["fullName"],
                    "IDENTIFIER": "$simulated@uma.money",
                    "USER_TYPE": "INDIVIDUAL",
                },
                "createdAt": timestamp(now),
                "destination": destination,
                "exchangeRate": rate as f64 / 1e6,
                "expiresAt": timestamp(expires),
                "feesIncluded": FEE_MICRO_USDB,
                "id": quote_id,
                "paymentInstructions": instructions,
                "platformFeesIncluded": 0,
                "receivingCurrency": currency_of(currency),
                "sendingCurrency": currency_of("USDB"),
                "source": { "cryptoNetwork": "SPARK", "currency": "USDB",
                            "sourceType": "REALTIME_FUNDING" },
                "status": "PENDING",
                "totalReceivingAmount": amount,
                "totalSendingAmount": cost,
                "transactionId": transaction_id,
                "simulated": true,
            });
            let transaction = json!({
                "createdAt": timestamp(now),
                "customerId": format!("Customer:{CUSTOMER}"),
                "description": body["description"],
                "destination": destination,
                "direction": "DEBIT",
                // Micro-USDB a minor unit, where the quote's is dollars a unit: as Grid has them.
                "exchangeRate": rate as f64 / per_unit as f64,
                "expectedSettlementAt": null,
                "fees": FEE_MICRO_USDB,
                "id": transaction_id,
                "paymentInstructions": instructions,
                "paymentRail": account["accountInfo"]["paymentRails"][0],
                "platformCustomerId": CUSTOMER,
                "platformFees": 0,
                "quoteId": quote_id,
                "railSelectionMode": "AUTO",
                "receivedAmount": { "amount": amount, "currency": currency_of(currency) },
                "sentAmount": { "amount": cost, "currency": currency_of("USDB") },
                "settlementTimelineSeconds": null,
                "source": { "currency": "USDB", "customerId": format!("Customer:{CUSTOMER}"),
                            "sourceType": "REALTIME_FUNDING" },
                "status": "PENDING",
                "type": "OUTGOING",
                "updatedAt": timestamp(now),
                "simulated": true,
            });
            let payout = Payout {
                quote: quote.clone(),
                transaction,
                ending: ending(&account["accountInfo"]),
                refund: self.uuid(),
                created: now,
                expires,
                funded: None,
            };
            state.transactions.insert(transaction_id, quote_id.clone());
            state.payouts.insert(quote_id, payout);
            (StatusCode::CREATED, quote)
        })
    }

    /// `POST /sandbox/send`: the quote's USDB arrives, and the payout is `PROCESSING` — until the
    /// payee's ending says what comes of it.
    pub fn fund(&self, key: Option<&str>, body: &Value) -> Reply {
        self.once("sandbox/send", key, |state| {
            let now = self.now();
            let quote_id = body["quoteId"].as_str().unwrap_or_default();
            let Some(payout) = state.payouts.get_mut(quote_id) else {
                return (StatusCode::NOT_FOUND, error("QUOTE_NOT_FOUND", "no such quote"));
            };
            if body["currencyCode"] != "USDB" {
                return invalid("currencyCode: this quote is funded in USDB".into());
            }
            match self.status(payout, now).0 {
                "PENDING" => {}
                "EXPIRED" => {
                    let why = "the quote expired unfunded";
                    return (StatusCode::BAD_REQUEST, error("QUOTE_EXPIRED", why));
                }
                _ => {
                    let why = "the quote was funded already";
                    return (StatusCode::CONFLICT, error("INVALID_STATE_TRANSITION", why));
                }
            }
            payout.funded = Some(now);
            (StatusCode::OK, self.transaction_at(payout, now))
        })
    }

    /// `GET /platform/external-accounts/{id}`.
    pub fn account(&self, id: &str) -> Reply {
        match self.inner.lock().unwrap().accounts.get(id) {
            Some(account) => (StatusCode::OK, account.clone()),
            None => (StatusCode::NOT_FOUND, error("ACCOUNT_NOT_FOUND", "no such account")),
        }
    }

    /// `GET /quotes/{id}`.
    pub fn quote(&self, id: &str) -> Reply {
        match self.inner.lock().unwrap().payouts.get(id) {
            Some(payout) => {
                let mut quote = payout.quote.clone();
                quote["status"] = self.status(payout, self.now()).0.into();
                (StatusCode::OK, quote)
            }
            None => (StatusCode::NOT_FOUND, error("QUOTE_NOT_FOUND", "no such quote")),
        }
    }

    /// `GET /transactions/{id}`: what the cosigner fetches.
    pub fn transaction(&self, id: &str) -> Reply {
        let state = self.inner.lock().unwrap();
        match state.transactions.get(id).and_then(|quote| state.payouts.get(quote)) {
            Some(payout) => (StatusCode::OK, self.transaction_at(payout, self.now())),
            None => (StatusCode::NOT_FOUND, error("TRANSACTION_NOT_FOUND", "no such transaction")),
        }
    }

    /// `GET /discoveries`: the names taken as a `bankName`, for a country and currency or all.
    pub fn discoveries(&self, country: Option<&str>, currency: Option<&str>) -> Reply {
        let data: Vec<Value> = corridors::CORRIDORS
            .iter()
            .filter(|c| country.is_none_or(|x| x == c.country))
            .filter(|c| currency.is_none_or(|x| x == c.currency))
            .flat_map(|c| {
                names(c).map(move |name| {
                    json!({ "bankName": name, "country": c.country, "currency": c.currency,
                            "displayName": name, "simulated": true })
                })
            })
            .collect();
        (StatusCode::OK, json!({ "data": data, "simulated": true }))
    }

    /// Where a payout has got to at `now`, and since when.
    fn status(&self, payout: &Payout, now: u64) -> (&'static str, u64) {
        let Some(funded) = payout.funded else {
            return if now >= payout.expires {
                ("EXPIRED", payout.expires)
            } else {
                ("PENDING", payout.created)
            };
        };
        let after = self.config.complete_after_secs;
        let t = now.saturating_sub(funded);
        match payout.ending.as_str() {
            "002" if t >= after => ("FAILED", funded + after),
            "003" if t >= 10 * after => ("COMPLETED", funded + 10 * after),
            "005" if t >= 2 * after => ("FAILED", funded + 2 * after),
            "002" | "003" => ("PROCESSING", funded),
            _ if t >= after => ("COMPLETED", funded + after),
            _ => ("PROCESSING", funded),
        }
    }

    /// The transaction as it stands at `now`.
    fn transaction_at(&self, payout: &Payout, now: u64) -> Value {
        let (status, since) = self.status(payout, now);
        let mut tx = payout.transaction.clone();
        tx["status"] = status.into();
        tx["updatedAt"] = timestamp(since).into();
        if matches!(status, "COMPLETED" | "FAILED") {
            tx["settledAt"] = timestamp(since).into();
        }
        if status == "FAILED" {
            let returned = payout.ending == "005";
            let reason = if returned { "PAYOUT_RETURNED" } else { "QUOTE_EXECUTION_FAILED" };
            tx["failureReason"] = reason.into();
            // To the platform's Grid balance: a just-in-time payout is refunded there, not to the
            // wallet that funded it.
            tx["refund"] = json!({
                "initiatedAt": timestamp(since),
                "reason": "TRANSACTION_FAILED",
                "reference": payout.refund,
                "settledAt": timestamp(since),
                "status": "COMPLETED",
            });
        }
        tx
    }

    /// What `act` answers — or, to an `Idempotency-Key` this route has seen, what it answered then.
    fn once(&self, route: &str, key: Option<&str>, act: impl FnOnce(&mut Inner) -> Reply) -> Reply {
        let mut state = self.inner.lock().unwrap();
        let Some(key) = key else {
            return act(&mut state);
        };
        let key = format!("{route} {key}");
        if let Some(first) = state.replies.get(&key) {
            return first.clone();
        }
        let reply = act(&mut state);
        state.replies.insert(key, reply.clone());
        reply
    }

    fn token(&self, headers: &HeaderMap) -> Option<Token> {
        use bitcoin::base64::Engine;
        let encoded = headers.get(AUTHORIZATION)?.to_str().ok()?.strip_prefix("Basic ")?;
        let decoded = bitcoin::base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
        let presented = String::from_utf8(decoded).ok()?;
        if presented == self.config.transact {
            Some(Token::Transact)
        } else if presented == self.config.view {
            Some(Token::View)
        } else {
            None
        }
    }
}

/// A payee for tests and demos that the fake takes on `rail`, whose account or phone number ends
/// in `ending`.
pub fn example_fields(
    corridor: &'static Corridor,
    rail: &Rail,
    ending: &str,
) -> BTreeMap<String, String> {
    rail.fields
        .iter()
        .map(|f| {
            let value = match (f.digits, f.options) {
                (Some(d), _) => {
                    let n = d.max.min(10).max(d.min);
                    format!("{}{ending:0>n$}", f.prefix.unwrap_or(""))
                }
                (_, Some(options)) => options[0].to_string(),
                _ => names(corridor).next().unwrap_or_default().to_string(),
            };
            (f.key.to_string(), value)
        })
        .collect()
}

/// The names the fake takes as a `bankName` in a corridor: its banks, then the networks its rails
/// offer by name.
fn names(corridor: &'static Corridor) -> impl Iterator<Item = &'static str> {
    let banks = BANKS
        .iter()
        .filter(move |(country, _)| *country == corridor.country)
        .flat_map(|(_, names)| names.iter().copied());
    let networks = corridor
        .rails
        .iter()
        .flat_map(|r| r.fields)
        .filter_map(|f| f.options)
        .flatten()
        .copied();
    banks.chain(networks)
}

/// The last three digits of an account's number, or of its phone number on mobile money.
fn ending(account_info: &Value) -> String {
    let id = account_info["accountNumber"]
        .as_str()
        .or(account_info["phoneNumber"].as_str())
        .unwrap_or_default();
    id[id.len().saturating_sub(3)..].to_string()
}

fn payment_rail(rail: &Rail) -> &'static str {
    if rail.rail == "bank" {
        "BANK_TRANSFER"
    } else {
        "MOBILE_MONEY"
    }
}

fn decimals(currency: &str) -> u32 {
    if currency == "USDB" {
        return 6;
    }
    corridors::CORRIDORS
        .iter()
        .find(|c| c.currency == currency)
        .map_or(2, |c| c.decimals)
}

/// A currency as Grid describes one.
fn currency_of(code: &str) -> Value {
    let (name, symbol) = match code {
        "USDB" => ("USDB", "$"),
        "NGN" => ("Nigerian Naira", "₦"),
        "KES" => ("Kenyan Shilling", "KSh"),
        "GHS" => ("Ghanaian Cedi", "GH₵"),
        "ZAR" => ("South African Rand", "R"),
        _ => (code, code),
    };
    json!({ "code": code, "decimals": decimals(code), "name": name, "symbol": symbol })
}

/// What one unit of a currency costs in micro-USDB. Made up, and steady, so that a quote asked
/// for again costs the same.
fn micro_usdb_per_unit(currency: &str) -> u64 {
    match currency {
        "NGN" => 736,
        "KES" => 7_750,
        "GHS" => 64_500,
        "ZAR" => 55_500,
        _ => 1_000_000,
    }
}

/// One of Grid's timestamps: `2026-09-27T00:00:05.000000Z`.
pub fn timestamp(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's `civil_from_days`, from a year starting in March.
    let z = days + 719_468;
    let (era, doe) = (z / 146_097, z % 146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let (h, m, s) = (rest / 3_600, rest % 3_600 / 60, rest % 60);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.000000Z")
}

fn error(code: &str, reason: &str) -> Value {
    json!({ "code": code, "reason": reason, "simulated": true })
}

fn invalid(reason: String) -> Reply {
    (StatusCode::BAD_REQUEST, error("INVALID_INPUT", &reason))
}

// ===============================================================================================
// Serving it
// ===============================================================================================

pub fn router(fake: Arc<FakeGrid>) -> Router {
    let grid = Router::new()
        .route("/platform/external-accounts", post(create_account))
        .route("/platform/external-accounts/{id}", get(read_account))
        .route("/quotes", post(create_quote))
        .route("/quotes/{id}", get(read_quote))
        .route("/sandbox/send", post(fund))
        .route("/transactions/{id}", get(read_transaction))
        .route("/discoveries", get(discoveries))
        .layer(middleware::from_fn_with_state(Arc::clone(&fake), guard))
        .with_state(fake);
    Router::new().nest(GRID_API, grid)
}

/// HTTP Basic on everything, the VIEW token only to read — and the label on every reply.
async fn guard(State(fake): State<Arc<FakeGrid>>, request: Request, next: Next) -> Response {
    let mut response = match fake.token(request.headers()) {
        None => reply((
            StatusCode::UNAUTHORIZED,
            error("UNAUTHORIZED", "HTTP Basic, with a token this fake Grid was given"),
        )),
        Some(Token::View) if request.method() != Method::GET => reply((
            StatusCode::FORBIDDEN,
            error("FORBIDDEN", "a VIEW token only reads"),
        )),
        Some(_) => next.run(request).await,
    };
    response
        .headers_mut()
        .insert(SIMULATED_HEADER, HeaderValue::from_static("true"));
    response
}

fn reply((status, body): Reply) -> Response {
    (status, Json(body)).into_response()
}

fn idempotency_key(headers: &HeaderMap) -> Option<&str> {
    headers.get("idempotency-key")?.to_str().ok()
}

async fn create_account(
    State(fake): State<Arc<FakeGrid>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    reply(fake.create_account(idempotency_key(&headers), &body))
}

async fn create_quote(
    State(fake): State<Arc<FakeGrid>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    reply(fake.create_quote(idempotency_key(&headers), &body))
}

async fn fund(
    State(fake): State<Arc<FakeGrid>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    reply(fake.fund(idempotency_key(&headers), &body))
}

async fn read_account(State(fake): State<Arc<FakeGrid>>, Path(id): Path<String>) -> Response {
    reply(fake.account(&id))
}

async fn read_quote(State(fake): State<Arc<FakeGrid>>, Path(id): Path<String>) -> Response {
    reply(fake.quote(&id))
}

async fn read_transaction(State(fake): State<Arc<FakeGrid>>, Path(id): Path<String>) -> Response {
    reply(fake.transaction(&id))
}

async fn discoveries(
    State(fake): State<Arc<FakeGrid>>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let get = |name: &str| query.get(name).map(String::as_str);
    reply(fake.discoveries(get("country"), get("currency")))
}

#[cfg(test)]
mod tests {
    use std::future::IntoFuture;

    use super::*;
    use crate::grid;

    const AFTER: u64 = 5;
    const TTL: u64 = 180;
    const TAG: &str = "deal-0c4e8d21a9f3b7e6";

    fn fake() -> Arc<FakeGrid> {
        FakeGrid::manual(Config {
            transact: "platform:transact".into(),
            view: "enclave:view".into(),
            complete_after_secs: AFTER,
            quote_ttl_secs: TTL,
        })
    }

    /// A payee on `country`'s first rail whose number ends in `ending`, as registered.
    fn account(fake: &FakeGrid, country: &str, ending: &str) -> Reply {
        let corridor = corridors::country(country).unwrap();
        let rail = &corridor.rails[0];
        let fields = example_fields(corridor, rail, ending);
        let body = grid::account_body(corridor.currency, rail.account_type, &fields, "Ada Obi");
        fake.create_account(None, &body)
    }

    /// A quote for a Nigerian payee ending in `ending`: the quote's id, and its transaction's.
    fn quoted(fake: &FakeGrid, ending: &str) -> (String, String) {
        let account = account(fake, "NG", ending).1;
        let body = grid::quote_body(account["id"].as_str().unwrap(), 3_000_000, TAG);
        let quote = fake.create_quote(None, &body).1;
        let id = |key: &str| quote[key].as_str().unwrap().to_string();
        (id("id"), id("transactionId"))
    }

    /// The transaction as the cosigner would read it, checking the quote says the same.
    fn read(fake: &FakeGrid, (quote, transaction): &(String, String)) -> Value {
        let tx = fake.transaction(transaction).1;
        assert_eq!(fake.quote(quote).1["status"], tx["status"], "the quote follows its payout");
        tx
    }

    fn fund(fake: &FakeGrid, (quote, _): &(String, String)) -> Reply {
        fake.fund(None, &grid::fund_body(quote))
    }

    /// A record's keys all the way down, and what kind of value each is.
    fn shape(value: &Value) -> Value {
        match value {
            Value::Object(o) => Value::Object(
                o.iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "simulated" | "_fixture"))
                    .map(|(k, v)| (k.clone(), shape(v)))
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().take(1).map(shape).collect()),
            Value::Number(n) if n.is_f64() => "float".into(),
            Value::Number(_) => "integer".into(),
            Value::String(_) => "string".into(),
            Value::Bool(_) => "bool".into(),
            Value::Null => "null".into(),
        }
    }

    fn recorded(json: &str) -> Value {
        shape(&serde_json::from_str(json).unwrap())
    }

    /// Every record is shaped like the one Grid's sandbox answered, key for key.
    #[test]
    fn every_record_is_shaped_like_grids() {
        let fake = fake();
        let (status, payee) = account(&fake, "NG", "789");
        assert_eq!(status, StatusCode::CREATED);
        let registered = include_str!("../tests/fixtures/external_account.json");
        assert_eq!(shape(&payee), recorded(registered));

        let body = grid::quote_body(payee["id"].as_str().unwrap(), 3_000_000, TAG);
        let (status, quote) = fake.create_quote(None, &body);
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(shape(&quote), recorded(include_str!("../tests/fixtures/quote.json")));

        let id = |key: &str| quote[key].as_str().unwrap().to_string();
        let ids = (id("id"), id("transactionId"));
        let pending = include_str!("../tests/fixtures/transaction_pending.json");
        assert_eq!(shape(&read(&fake, &ids)), recorded(pending));
        assert_eq!(fund(&fake, &ids).0, StatusCode::OK);
        fake.advance(AFTER);
        let completed = include_str!("../tests/fixtures/transaction.json");
        assert_eq!(shape(&read(&fake, &ids)), recorded(completed));

        let failing = quoted(&fake, "002");
        fund(&fake, &failing);
        fake.advance(AFTER);
        let failed = include_str!("../tests/fixtures/transaction_failed.json");
        assert_eq!(shape(&read(&fake, &failing)), recorded(failed));

        let banks = fake.discoveries(Some("NG"), Some("NGN")).1;
        let listed = recorded(include_str!("../tests/fixtures/discoveries.json"));
        assert_eq!(shape(&banks), listed);
    }

    /// Straight after quoting, the transaction already names the deal, the payee and the amount,
    /// in the payee's currency: only its status is left to change.
    #[test]
    fn a_quote_is_everything_the_cosigner_checks_but_the_status() {
        for (corridor, rail) in corridors::rails() {
            let fake = fake();
            let fields = example_fields(corridor, rail, "789");
            let body = grid::account_body(corridor.currency, rail.account_type, &fields, "Ada Obi");
            let payee = fake.create_account(None, &body).1;
            let account_id = payee["id"].as_str().unwrap();
            for (field, value) in &fields {
                assert_eq!(payee["accountInfo"][field], value.as_str(), "{field} comes back");
            }
            let body = grid::quote_body(account_id, rail.min_minor, TAG);
            let quote = fake.create_quote(None, &body).1;
            let tx = fake.transaction(quote["transactionId"].as_str().unwrap()).1;
            assert_eq!(tx["id"], quote["transactionId"]);
            assert_eq!(tx["type"], "OUTGOING");
            assert_eq!(tx["description"], TAG);
            assert_eq!(tx["destination"]["accountId"], account_id);
            assert_eq!(tx["receivedAmount"]["currency"]["code"], corridor.currency);
            assert_eq!(tx["receivedAmount"]["amount"], rail.min_minor);
            assert_eq!(tx["status"], "PENDING");
            assert_eq!(quote["sendingCurrency"]["code"], "USDB");
            assert!(grid::unix_secs(quote["expiresAt"].as_str().unwrap()).is_some());
        }
    }

    /// The corridor's rules, as Grid applies them: a malformed field, a bank Grid does not list,
    /// or no payee is refused.
    #[test]
    fn each_rail_is_checked_as_grid_checks_it() {
        let fake = fake();
        for (corridor, rail) in corridors::rails() {
            let fields = example_fields(corridor, rail, "789");
            let body = |fields: &BTreeMap<String, String>| {
                grid::account_body(corridor.currency, rail.account_type, fields, "Ada Obi")
            };
            assert_eq!(fake.create_account(None, &body(&fields)).0, StatusCode::CREATED);
            for field in fields.keys() {
                let mut broken = fields.clone();
                broken.insert(field.clone(), "x".into());
                let (status, why) = fake.create_account(None, &body(&broken));
                assert_eq!(status, StatusCode::BAD_REQUEST, "{} {field}", corridor.country);
                assert_eq!(why["code"], "INVALID_INPUT");
            }
            let mut anonymous = body(&fields);
            anonymous["accountInfo"]["beneficiary"]["fullName"] = "".into();
            assert_eq!(fake.create_account(None, &anonymous).0, StatusCode::BAD_REQUEST);
            let mut elsewhere = body(&fields);
            elsewhere["currency"] = "USD".into();
            assert_eq!(fake.create_account(None, &elsewhere).0, StatusCode::BAD_REQUEST);
        }
        // A well-formed name Grid does not list.
        let ng = corridors::country("NG").unwrap();
        let mut fields = example_fields(ng, &ng.rails[0], "789");
        fields.insert("bankName".into(), "Bank of Nowhere".into());
        let body = grid::account_body("NGN", "NGN_ACCOUNT", &fields, "Ada Obi");
        assert_eq!(fake.create_account(None, &body).0, StatusCode::BAD_REQUEST);
    }

    /// The name check goes by the payee's ending: an account number's, or a phone number's.
    #[test]
    fn the_payees_ending_decides_the_name_check() {
        let fake = fake();
        for (ending, check) in [
            ("102", "NOT_MATCHED"),
            ("103", "PARTIAL_MATCH"),
            ("104", "PENDING"),
            ("106", "UNSUPPORTED"),
            ("107", "CHECKED_BY_RECEIVING_FI"),
            ("789", "MATCHED"),
        ] {
            for country in ["NG", "KE"] {
                let payee = account(&fake, country, ending).1;
                assert_eq!(payee["beneficiaryVerificationStatus"], check, "{country} {ending}");
            }
        }
        let partial = account(&fake, "NG", "103").1;
        assert_ne!(partial["beneficiaryVerifiedData"]["fullName"], "Ada Obi", "another name");
        assert_eq!(account(&fake, "NG", "789").1["beneficiaryVerifiedData"]["fullName"], "Ada Obi");
        assert_eq!(account(&fake, "NG", "105").0, StatusCode::BAD_REQUEST);
    }

    /// PENDING until funded, PROCESSING for a while, then what the payee's ending says.
    #[test]
    fn the_payees_ending_decides_the_payout() {
        let fake = fake();
        let fine = quoted(&fake, "789");
        let failing = quoted(&fake, "002");
        let slow = quoted(&fake, "003");
        let returned = quoted(&fake, "005");
        for payout in [&fine, &failing, &slow, &returned] {
            assert_eq!(read(&fake, payout)["status"], "PENDING");
            let (status, tx) = fund(&fake, payout);
            assert_eq!((status, tx["status"].as_str()), (StatusCode::OK, Some("PROCESSING")));
        }
        fake.advance(AFTER - 1);
        assert_eq!(read(&fake, &fine)["status"], "PROCESSING");
        fake.advance(1);
        let done = read(&fake, &fine);
        assert_eq!(done["status"], "COMPLETED");
        assert!(done["settledAt"].is_string() && done.get("failureReason").is_none());

        let failed = read(&fake, &failing);
        assert_eq!(failed["status"], "FAILED");
        assert_eq!(failed["failureReason"], "QUOTE_EXECUTION_FAILED");
        assert_eq!(failed["refund"]["status"], "COMPLETED");
        assert_eq!(read(&fake, &slow)["status"], "PROCESSING");
        assert_eq!(read(&fake, &returned)["status"], "COMPLETED");

        fake.advance(AFTER);
        let bounced = read(&fake, &returned);
        assert_eq!(bounced["status"], "FAILED");
        assert_eq!(bounced["failureReason"], "PAYOUT_RETURNED");
        fake.advance(8 * AFTER);
        assert_eq!(read(&fake, &slow)["status"], "COMPLETED");
        assert_eq!(read(&fake, &fine)["status"], "COMPLETED", "and stays so");
    }

    /// A quote nobody paid for expires, and then cannot be paid for; nor can one be paid twice.
    #[test]
    fn a_quote_is_paid_for_once_and_only_in_time() {
        let fake = fake();
        let lapsing = quoted(&fake, "789");
        let paid = quoted(&fake, "789");
        assert_eq!(fund(&fake, &paid).0, StatusCode::OK);
        assert_eq!(fund(&fake, &paid).1["code"], "INVALID_STATE_TRANSITION");
        fake.advance(TTL - 1);
        assert_eq!(read(&fake, &lapsing)["status"], "PENDING");
        fake.advance(1);
        assert_eq!(read(&fake, &lapsing)["status"], "EXPIRED");
        let (status, why) = fund(&fake, &lapsing);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(why["code"], "QUOTE_EXPIRED");
        assert_eq!(read(&fake, &paid)["status"], "COMPLETED", "a funded quote does not expire");
        assert_eq!(fake.transaction("Transaction:nope").0, StatusCode::NOT_FOUND);
    }

    /// The same key again is the first answer, whatever is asked the second time.
    #[test]
    fn a_repeated_key_gets_the_first_answer() {
        let fake = fake();
        let ng = corridors::country("NG").unwrap();
        let body = |ending| {
            let fields = example_fields(ng, &ng.rails[0], ending);
            grid::account_body("NGN", "NGN_ACCOUNT", &fields, "Ada Obi")
        };
        let first = fake.create_account(Some("k1"), &body("789"));
        assert_eq!(fake.create_account(Some("k1"), &body("789")), first);
        assert_eq!(fake.create_account(Some("k1"), &body("456")), first);
        assert_ne!(fake.create_account(Some("k2"), &body("789")).1["id"], first.1["id"]);
        // Keys are per route: the same key on quotes is a new one there.
        let account_id = first.1["id"].as_str().unwrap();
        let quote = fake.create_quote(Some("k1"), &grid::quote_body(account_id, 3_000_000, TAG));
        assert_eq!(quote.0, StatusCode::CREATED);
        let again = fake.create_quote(Some("k1"), &grid::quote_body(account_id, 1, "other"));
        assert_eq!(again, quote);
    }

    /// Everything served says it is simulated.
    #[test]
    fn every_record_is_labelled_simulated() {
        let fake = fake();
        let payout = quoted(&fake, "789");
        let records = [
            fake.account("ExternalAccount:00000000-0000-4000-8000-000000000001").1,
            fake.quote(&payout.0).1,
            fund(&fake, &payout).1,
            read(&fake, &payout),
            fake.transaction("Transaction:nope").1,
            fake.discoveries(None, None).1,
        ];
        for record in &records {
            assert_eq!(record["simulated"], true, "{record}");
        }
        let listed = &records[5]["data"];
        assert!(listed.as_array().unwrap().iter().all(|entry| entry["simulated"] == true));
    }

    #[test]
    fn ids_are_counted_in_tests_and_random_otherwise() {
        let counted = fake();
        assert_eq!(counted.uuid(), "00000000-0000-4000-8000-000000000001");
        assert_eq!(counted.uuid(), "00000000-0000-4000-8000-000000000002");
        let random = FakeGrid::new(counted.config.clone());
        let (a, b) = (random.uuid(), random.uuid());
        assert_ne!(a, b);
        let parts: Vec<usize> = a.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
        assert!(a.as_bytes()[14] == b'4', "version 4: {a}");
    }

    /// The fake's timestamps are Grid's, and read back to the second.
    #[test]
    fn a_timestamp_reads_back() {
        assert_eq!(timestamp(EPOCH), "2026-09-27T00:00:00.000000Z");
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000000Z");
        for secs in [0, EPOCH, EPOCH + 86_399, 951_782_400, 1_709_208_000, 4_107_542_400] {
            assert_eq!(grid::unix_secs(&timestamp(secs)), Some(secs), "{}", timestamp(secs));
        }
    }

    /// HTTP Basic on every route: the TRANSACT token for anything, the VIEW token only to read.
    /// Every reply says it is simulated.
    #[tokio::test]
    async fn the_tokens_decide_who_may_do_what() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}{GRID_API}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, router(fake())).into_future());
        let http = reqwest::Client::new();
        let ng = corridors::country("NG").unwrap();
        let fields = example_fields(ng, &ng.rails[0], "789");
        let payee = grid::account_body("NGN", "NGN_ACCOUNT", &fields, "Ada Obi");

        let asks = [
            (None, false, StatusCode::UNAUTHORIZED),
            (Some(("platform", "wrong")), false, StatusCode::UNAUTHORIZED),
            (Some(("enclave", "view")), true, StatusCode::FORBIDDEN),
            (Some(("enclave", "view")), false, StatusCode::OK),
            (Some(("platform", "transact")), true, StatusCode::CREATED),
            (Some(("platform", "transact")), false, StatusCode::OK),
        ];
        for (token, write, want) in asks {
            let request = if write {
                http.post(format!("{base}/platform/external-accounts")).json(&payee)
            } else {
                http.get(format!("{base}/discoveries?country=NG&currency=NGN"))
            };
            let request = match token {
                Some((id, secret)) => request.basic_auth(id, Some(secret)),
                None => request,
            };
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), want, "{token:?} write={write}");
            assert_eq!(response.headers()[SIMULATED_HEADER], "true");
        }
    }
}
