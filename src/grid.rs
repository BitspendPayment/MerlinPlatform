//! Lightspark Grid: the calls this platform makes, with its TRANSACT token.
//!
//! The enclave never sees this token. It reads the same records with its own VIEW-only one, so a
//! leak of the image's environment cannot move money.

use std::fmt;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::payout::{GRID_API, GRID_ORIGIN};

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
    client_id: String,
    client_secret: String,
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

impl Grid {
    pub fn new(client_id: String, client_secret: String) -> anyhow::Result<Self> {
        // Bounded, because the watcher asks about every payout in turn: one call that never
        // returned would stall all of them.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            client_id,
            client_secret,
        })
    }

    /// Register the payee's bank account under the platform.
    ///
    /// Every write carries an `Idempotency-Key`: the same key again returns Grid's first answer
    /// instead of doing the thing twice.
    pub async fn external_account(
        &self,
        key: &str,
        account_number: &str,
        bank_name: &str,
        full_name: &str,
    ) -> Result<ExternalAccount, GridError> {
        self.call(self.post("/platform/external-accounts", key).json(&serde_json::json!({
            "currency": "NGN",
            "accountInfo": {
                "accountType": "NGN_ACCOUNT",
                "accountNumber": account_number,
                "bankName": bank_name,
                "beneficiary": { "beneficiaryType": "INDIVIDUAL", "fullName": full_name },
            },
        })))
        .await
    }

    /// Quote the naira, funded just in time in USDB on Spark. `deal_tag` rides along as the
    /// description, which is how the cosigner tells this payout's deal from anybody else's.
    pub async fn quote(
        &self,
        key: &str,
        account_id: &str,
        kobo: i64,
        deal_tag: &str,
    ) -> Result<Quote, GridError> {
        self.call(self.post("/quotes", key).json(&serde_json::json!({
            "source": { "sourceType": "REALTIME_FUNDING", "currency": "USDB", "cryptoNetwork": "SPARK" },
            "destination": { "destinationType": "ACCOUNT", "accountId": account_id },
            "lockedCurrencySide": "RECEIVING",
            "lockedCurrencyAmount": kobo,
            "description": deal_tag,
        })))
        .await
    }

    pub async fn quote_status(&self, quote_id: &str) -> Result<Quote, GridError> {
        let url = format!("{GRID_ORIGIN}{GRID_API}/quotes/{quote_id}");
        self.call(self.http.get(url)).await
    }

    /// Pay for the quote. Grid pays the bank on its own once the funds arrive.
    ///
    /// ponytail: sandbox only — Grid simulates the funds arriving. Production pays the quote's
    /// `paymentInstructions` (a Spark wallet) in USDB from the platform's treasury.
    pub async fn sandbox_fund(&self, key: &str, quote_id: &str) -> Result<(), GridError> {
        let request = self.post("/sandbox/send", key).json(&serde_json::json!({
            "quoteId": quote_id,
            "currencyCode": "USDB",
        }));
        let (status, body) = self.send(request).await?;
        if status.is_success() {
            return Ok(());
        }
        Err(refusal(status, &body))
    }

    fn post(&self, path: &str, key: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{GRID_ORIGIN}{GRID_API}{path}"))
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
}
