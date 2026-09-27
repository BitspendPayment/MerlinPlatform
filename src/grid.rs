//! Lightspark Grid: the calls this platform makes, with its TRANSACT token.
//!
//! The enclave never sees this token. It reads the same records with its own VIEW-only one, so a
//! leak of the image's environment cannot move money.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::payout::GRID_API;

/// Why a Grid call gave no answer.
#[derive(Debug, PartialEq)]
pub enum GridError {
    /// Grid understood the request and said no — a bank it does not list, an amount over a limit.
    /// The caller's to fix.
    Rejected(String),
    /// Grid could not be reached, failed, is throttling, or answered in a shape this does not
    /// understand. Worth trying again later.
    Unavailable(String),
}

impl fmt::Display for GridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GridError::Rejected(why) | GridError::Unavailable(why) => f.write_str(why),
        }
    }
}

/// A non-2xx answer, sorted by whose problem it is.
fn refusal(status: reqwest::StatusCode, body: &str) -> GridError {
    let why = format!("Grid answered {status}: {body}");
    // 408 and 429 are Grid's state, not the request's.
    if status.is_client_error() && status.as_u16() != 408 && status.as_u16() != 429 {
        GridError::Rejected(why)
    } else {
        GridError::Unavailable(why)
    }
}

pub struct Grid {
    http: reqwest::Client,
    /// Where this dials Grid: Grid itself, or a fake one. The enclave may reach it elsewhere — see
    /// `payout::Deal::grid_origin`.
    base: String,
    client_id: String,
    client_secret: String,
    /// Bank lists by country and currency, as Grid gave them.
    ///
    /// ponytail: kept for the life of the process; a TTL if Grid's lists change under a running
    /// platform.
    banks: Mutex<BTreeMap<String, Vec<Bank>>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalAccount {
    pub id: String,
    /// Grid's check of the name against the bank's records: `MATCHED`, `PARTIAL_MATCH`,
    /// `NOT_MATCHED`, … Absent where the rail has no such check.
    #[serde(default)]
    pub beneficiary_verification_status: Option<String>,
    #[serde(default)]
    pub beneficiary_verified_data: Option<VerifiedName>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedName {
    /// The account holder's name as the bank has it.
    #[serde(default)]
    pub full_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub id: String,
    pub status: String,
    pub expires_at: String,
    pub sending_currency: Currency,
    /// Smallest units of the sending currency: micro-USDB.
    pub total_sending_amount: u64,
    /// Created with the quote, and what the cosigner will be asked about.
    pub transaction_id: String,
}

#[derive(Debug, Deserialize)]
pub struct Currency {
    pub code: String,
}

/// A name Grid takes as a `bankName`: a bank, or a mobile-money network.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Bank {
    /// What to send, spelled exactly so.
    #[serde(rename(deserialize = "bankName"))]
    pub bank_name: String,
    /// What to show.
    #[serde(rename(deserialize = "displayName"))]
    pub display_name: String,
}

impl Grid {
    /// `base_url` is where Grid is dialled — `https://api.lightspark.com`, or a fake Grid — with
    /// no path: every call's path starts with the API version.
    pub fn new(base_url: &str, client_id: String, client_secret: String) -> anyhow::Result<Self> {
        // Bounded, because the watcher asks about every payout in turn: one call that never
        // returned would stall all of them.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
            client_id,
            client_secret,
            banks: Mutex::default(),
        })
    }

    /// Register the payee's account under the platform: `fields` of the corridor's `account_type`,
    /// in `currency` — see [`account_body`].
    ///
    /// Every write carries an `Idempotency-Key`: the same key again returns Grid's first answer
    /// instead of doing the thing twice.
    pub async fn external_account(
        &self,
        key: &str,
        currency: &str,
        account_type: &str,
        fields: &BTreeMap<String, String>,
        full_name: &str,
    ) -> Result<ExternalAccount, GridError> {
        let body = account_body(currency, account_type, fields, full_name);
        self.call(self.post("/platform/external-accounts", key).json(&body))
            .await
    }

    /// Quote `amount_minor` of the account's currency, funded just in time in USDB on Spark.
    /// `deal_tag` rides along as the description, which is how the cosigner tells this payout's
    /// deal from anybody else's.
    pub async fn quote(
        &self,
        key: &str,
        account_id: &str,
        amount_minor: i64,
        deal_tag: &str,
    ) -> Result<Quote, GridError> {
        let body = quote_body(account_id, amount_minor, deal_tag);
        self.call(self.post("/quotes", key).json(&body)).await
    }

    pub async fn quote_status(&self, quote_id: &str) -> Result<Quote, GridError> {
        let url = format!("{}{GRID_API}/quotes/{quote_id}", self.base);
        self.call(self.http.get(url)).await
    }

    /// Pay for the quote. Grid pays the payee on its own once the funds arrive.
    ///
    /// ponytail: sandbox only — Grid simulates the funds arriving. Production pays the quote's
    /// `paymentInstructions` (a Spark wallet) in USDB from the platform's treasury.
    pub async fn sandbox_fund(&self, key: &str, quote_id: &str) -> Result<(), GridError> {
        let request = self.post("/sandbox/send", key).json(&fund_body(quote_id));
        let (status, body) = self.send(request).await?;
        if status.is_success() {
            return Ok(());
        }
        Err(refusal(status, &body))
    }

    /// The names Grid takes as a `bankName` in `country`: its banks, and on some rails its
    /// mobile-money networks.
    pub async fn discoveries(&self, country: &str, currency: &str) -> Result<Vec<Bank>, GridError> {
        let key = format!("{country}/{currency}");
        if let Some(banks) = self.banks.lock().unwrap().get(&key) {
            return Ok(banks.clone());
        }
        #[derive(Deserialize)]
        struct Listed {
            data: Vec<Bank>,
        }
        let url = format!("{}{GRID_API}/discoveries", self.base);
        let request = self
            .http
            .get(url)
            .query(&[("country", country), ("currency", currency)]);
        let listed: Listed = self.call(request).await?;
        if !listed.data.is_empty() {
            self.banks.lock().unwrap().insert(key, listed.data.clone());
        }
        Ok(listed.data)
    }

    fn post(&self, path: &str, key: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{GRID_API}{path}", self.base))
            .header("Idempotency-Key", key)
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(reqwest::StatusCode, String), GridError> {
        let response = request
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .send()
            .await
            .map_err(|e| GridError::Unavailable(format!("reaching Grid: {e}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| GridError::Unavailable(format!("reading Grid's answer: {e}")))?;
        Ok((status, body))
    }

    async fn call<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, GridError> {
        let (status, body) = self.send(request).await?;
        if !status.is_success() {
            return Err(refusal(status, &body));
        }
        serde_json::from_str(&body).map_err(|e| {
            GridError::Unavailable(format!("Grid's answer was not the expected shape ({e}): {body}"))
        })
    }
}

/// `POST /platform/external-accounts`: the corridor's fields as the customer typed them. Neither
/// the account type nor the payee can be overridden by a field.
pub fn account_body(
    currency: &str,
    account_type: &str,
    fields: &BTreeMap<String, String>,
    full_name: &str,
) -> Value {
    let mut info: serde_json::Map<String, Value> =
        fields.iter().map(|(k, v)| (k.clone(), v.as_str().into())).collect();
    info.insert("accountType".into(), account_type.into());
    info.insert(
        "beneficiary".into(),
        json!({ "beneficiaryType": "INDIVIDUAL", "fullName": full_name }),
    );
    json!({ "currency": currency, "accountInfo": info })
}

/// `POST /quotes`: a set amount received, funded just in time in USDB on Spark.
pub fn quote_body(account_id: &str, amount_minor: i64, deal_tag: &str) -> Value {
    json!({
        "source": { "sourceType": "REALTIME_FUNDING", "currency": "USDB", "cryptoNetwork": "SPARK" },
        "destination": { "destinationType": "ACCOUNT", "accountId": account_id },
        "lockedCurrencySide": "RECEIVING",
        "lockedCurrencyAmount": amount_minor,
        "description": deal_tag,
    })
}

/// `POST /sandbox/send`: the quote's USDB, arriving.
pub fn fund_body(quote_id: &str) -> Value {
    json!({ "quoteId": quote_id, "currencyCode": "USDB" })
}

/// Seconds since the epoch at one of Grid's timestamps — `2026-09-26T19:59:44.870750Z` — or `None`
/// for anything else, including anything before 1970.
pub fn unix_secs(timestamp: &str) -> Option<u64> {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let number = |part: &str| digits(part).then(|| part.parse::<u64>().ok()).flatten();
    let at = timestamp.strip_suffix('Z')?;
    // A fraction of a second, if any, is dropped: an expiry is kept to the second.
    let (at, fraction) = at.split_once('.').unwrap_or((at, "0"));
    let (date, time) = at.split_once('T')?;
    if !digits(fraction) {
        return None;
    }
    let date: Vec<u64> = date.split('-').map(number).collect::<Option<_>>()?;
    let time: Vec<u64> = time.split(':').map(number).collect::<Option<_>>()?;
    let [y, m, d] = date[..] else { return None };
    let [hh, mm, ss] = time[..] else { return None };
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days since 1970-01-01: Howard Hinnant's `days_from_civil`, from a year starting in March.
    let y = if m <= 2 { y - 1 } else { y };
    let (era, yoe) = (y / 400, y % 400);
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146_097 + doe).checked_sub(719_468)?;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use super::*;

    #[test]
    fn grids_refusals_are_sorted_by_whose_problem_they_are() {
        for code in [400, 403, 404, 409, 422] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(matches!(refusal(status, ""), GridError::Rejected(_)), "{code}");
        }
        for code in [408, 429, 500, 502, 503] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(matches!(refusal(status, ""), GridError::Unavailable(_)), "{code}");
        }
    }

    /// Grid's timestamps, as recorded in the sandbox, and nothing that only looks like one.
    #[test]
    fn a_grid_timestamp_is_read_to_the_second() {
        assert_eq!(unix_secs("2026-09-26T19:59:44.870750Z"), Some(1_790_452_784));
        assert_eq!(unix_secs("2026-09-26T19:59:44Z"), Some(1_790_452_784));
        assert_eq!(unix_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_secs("2024-02-29T12:00:00.5Z"), Some(1_709_208_000));
        for not in [
            "2026-09-26T19:59:44",
            "2026-09-26T19:59:44+01:00",
            "2026-09-26 19:59:44Z",
            "2026-13-26T19:59:44Z",
            "2026-09-26T19:59:44.Z",
            "1969-12-31T23:59:59Z",
            "",
        ] {
            assert_eq!(unix_secs(not), None, "{not}");
        }
    }

    /// The customer's fields cannot pass for the account type or the payee.
    #[test]
    fn a_field_cannot_overwrite_the_account_type_or_the_payee() {
        let fields: BTreeMap<String, String> = [
            ("accountNumber", "0123456789"),
            ("accountType", "USD_ACCOUNT"),
            ("beneficiary", "someone else"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let body = account_body("NGN", "NGN_ACCOUNT", &fields, "Ada Obi");
        assert_eq!(body["accountInfo"]["accountType"], "NGN_ACCOUNT");
        assert_eq!(body["accountInfo"]["beneficiary"]["fullName"], "Ada Obi");
        assert_eq!(body["accountInfo"]["accountNumber"], "0123456789");
    }
}
