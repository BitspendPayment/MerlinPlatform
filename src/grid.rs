//! Lightspark Grid: the four calls this platform makes, with its TRANSACT token.
//!
//! The enclave never sees this token. It reads the same records with its own VIEW-only one, so a
//! leak of the image's environment cannot move money.

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::payout::{GRID_API, GRID_ORIGIN};

pub struct Grid {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
}

#[derive(Debug, Deserialize)]
pub struct ExternalAccount {
    pub id: String,
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
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            client_id,
            client_secret,
        }
    }

    /// Register the payee's bank account under the platform.
    pub async fn external_account(
        &self,
        account_number: &str,
        bank_name: &str,
        full_name: &str,
    ) -> Result<ExternalAccount, String> {
        self.call(self.post("/platform/external-accounts").json(&serde_json::json!({
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
    pub async fn quote(&self, account_id: &str, kobo: i64, deal_tag: &str) -> Result<Quote, String> {
        self.call(self.post("/quotes").json(&serde_json::json!({
            "source": { "sourceType": "REALTIME_FUNDING", "currency": "USDB", "cryptoNetwork": "SPARK" },
            "destination": { "destinationType": "ACCOUNT", "accountId": account_id },
            "lockedCurrencySide": "RECEIVING",
            "lockedCurrencyAmount": kobo,
            "description": deal_tag,
        })))
        .await
    }

    pub async fn quote_status(&self, quote_id: &str) -> Result<Quote, String> {
        let url = format!("{GRID_ORIGIN}{GRID_API}/quotes/{quote_id}");
        self.call(self.http.get(url)).await
    }

    /// Pay for the quote. Grid pays the bank on its own once the funds arrive.
    ///
    /// ponytail: sandbox only — Grid simulates the funds arriving. Production pays the quote's
    /// `paymentInstructions` (a Spark wallet) in USDB from the platform's treasury.
    pub async fn sandbox_fund(&self, quote_id: &str) -> Result<(), String> {
        let response = self
            .authed(self.post("/sandbox/send").json(&serde_json::json!({
                "quoteId": quote_id,
                "currencyCode": "USDB",
            })))
            .send()
            .await
            .map_err(|e| format!("reaching Grid: {e}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(format!(
            "Grid answered {status}: {}",
            response.text().await.unwrap_or_default()
        ))
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.post(format!("{GRID_ORIGIN}{GRID_API}{path}"))
    }

    fn authed(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.basic_auth(&self.client_id, Some(&self.client_secret))
    }

    async fn call<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T, String> {
        let response = self
            .authed(request)
            .send()
            .await
            .map_err(|e| format!("reaching Grid: {e}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("reading Grid's answer: {e}"))?;
        if !status.is_success() {
            return Err(format!("Grid answered {status}: {body}"));
        }
        serde_json::from_str(&body).map_err(|e| format!("Grid's answer was not the expected shape ({e}): {body}"))
    }
}
