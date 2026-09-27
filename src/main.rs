//! Merlin Platform — send money to a Nigerian bank account.
//!
//! ```text
//!   GRID_CLIENT_ID=… GRID_CLIENT_SECRET=… cargo run -- --sats-per-usd 1000 \
//!       --asp http://127.0.0.1:7070 --payout-xonly <32-byte hex> --store ./platform-state.json
//! ```
//!
//! It pays first and is reimbursed after — `examples/card-escrow` in MerlinWallet, with a Grid
//! payout where the card purchase was:
//!
//! 1. `POST /payouts` — the app names a bank account, an amount in kobo and its own deal tag. The
//!    platform registers the account with Grid, quotes the payout funded just in time in USDB,
//!    prices it in sats, and answers with the policy the app seals into the customer's escrow.
//! 2. `POST /payouts/{request_id}/fund` — the app has sealed it. The platform asks the cosigner to
//!    reimburse it *before* paying, expecting exactly one refusal: the payout has not completed.
//!    Only then does it fund the quote.
//! 3. [`watch`] polls Grid. When the payout completes the platform asks again, and the cosigner —
//!    having fetched the payout and the payee's account for itself — co-signs its reimbursement.
//!
//! The enclave-facing routes (`/escrow/stream`, `/escrow/send`, `/pair/wallet`, `/status`) are
//! `escrow-service`'s.

mod grid;
mod payout;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use clap::Parser;
use cosigner::evidence::safe_reference;
use escrow_service::reimburse::{self, Asked};
use escrow_service::wire::{router as wire_router, Connections, Wire};
use escrow_service::{Reimbursement, Service, Stage};
use serde::Deserialize;
use serde_json::json;
use threshold::identifier::Identifier;

use grid::Grid;
use payout::{policy_for, price_sats, Deal, PREFLIGHT_REFUSAL};

/// How often [`watch`] asks Grid about payouts in flight.
const WATCH_EVERY: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(about = "Send to a bank: paid out through Lightspark Grid, reimbursed from a Bitcoin escrow.")]
struct Args {
    #[arg(long, default_value_t = 7200)]
    port: u16,
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// The ASP, for reading what an escrow holds and submitting what was approved.
    #[arg(long, env = "ASP_URL", default_value = "http://127.0.0.1:7070")]
    asp: String,
    /// This platform's own key, x-only hex — where it is reimbursed.
    #[arg(long, env = "PLATFORM_PAYOUT_XONLY")]
    payout_xonly: String,
    /// The label the enclave's image knows this platform by.
    #[arg(long, env = "PLATFORM_LABEL", default_value = "merlin-platform")]
    label: String,
    /// Where to keep what must survive a restart. Its half of every escrow key lives here.
    #[arg(long, env = "PLATFORM_STORE")]
    store: Option<std::path::PathBuf>,
    /// What the platform charges, in sats, per US dollar a payout costs it at Grid.
    ///
    /// ponytail: one fixed rate, for the sandbox. Production prices from a BTC/USD feed, with
    /// the platform's margin in it.
    #[arg(long, env = "PLATFORM_SATS_PER_USD")]
    sats_per_usd: u64,
    /// A Grid token that can TRANSACT. Never the one in the enclave's image.
    #[arg(long, env = "GRID_CLIENT_ID", hide_env_values = true)]
    grid_client_id: String,
    #[arg(long, env = "GRID_CLIENT_SECRET", hide_env_values = true)]
    grid_client_secret: String,
}

struct App {
    wire: Arc<Wire>,
    grid: Grid,
    /// Where this platform is paid, as the script an output pays.
    payout_script: String,
    sats_per_usd: u64,
    /// Payouts being funded right now. Two calls that both saw one unfunded would both pay for it.
    funding: std::sync::Mutex<BTreeSet<String>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    // Where this platform is paid, derived from its own key. The sealed policy names its script
    // as the only place a release may go.
    let mut asp = ark::client::AspClient::connect(&args.asp)
        .await
        .map_err(|e| anyhow::anyhow!("connecting to the ASP at {}: {e}", args.asp))?;
    let info = asp
        .get_info()
        .await
        .map_err(|e| anyhow::anyhow!("asking the ASP what it is: {e}"))?;
    let network =
        ark::client::parse_network(&info.network).map_err(|e| anyhow::anyhow!("{e}"))?;
    let payout = ark::client::ark_address(
        &args.payout_xonly,
        &info.signer_pubkey,
        info.unilateral_exit_delay as u32,
        network,
    )
    .map_err(|e| anyhow::anyhow!("deriving where this platform is paid: {e}"))?;
    let payout_script =
        ark::client::ark_address_script_pubkey_hex(&payout).map_err(|e| anyhow::anyhow!("{e}"))?;

    let identifier =
        Identifier::derive(args.label.as_bytes()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let service = Service::new(
        identifier.clone(),
        payout.clone(),
        args.asp.clone(),
        args.store.clone(),
    );
    service.restore().await?;
    let wire = Arc::new(Wire {
        service: Arc::clone(&service),
        connections: Arc::new(Connections::default()),
    });

    // Anything a previous run left owed, asked for again; then keep asking. See `reimburse`.
    for (request_id, outcome) in reimburse::resume(&wire).await {
        tracing::info!(%request_id, ?outcome, "picked up where a previous run left off");
    }
    tokio::spawn(reimburse::keep_trying(Arc::clone(&wire)));

    let app = Arc::new(App {
        wire: Arc::clone(&wire),
        grid: Grid::new(args.grid_client_id, args.grid_client_secret),
        payout_script,
        sats_per_usd: args.sats_per_usd,
        funding: Default::default(),
    });
    tokio::spawn(watch(Arc::clone(&app)));

    let router = wire_router(Arc::clone(&service), Arc::clone(&wire.connections)).merge(
        Router::new()
            .route("/payouts", post(quote))
            .route("/payouts/{request_id}/fund", post(fund))
            .with_state(app),
    );
    let listener = tokio::net::TcpListener::bind((args.bind.as_str(), args.port)).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        identifier = %hex::encode(identifier.serialize()),
        payout = %payout,
        "platform up — Grid SANDBOX payouts, real Bitcoin escrow"
    );
    axum::serve(listener, router).await?;
    Ok(())
}

#[derive(Deserialize)]
struct PayoutRequest {
    escrow_key: String,
    account_number: String,
    bank_name: String,
    full_name: String,
    /// Naira, in kobo. An integer, so nothing is lost on the way in.
    amount_kobo: i64,
    /// Chosen by the app, never by this platform — see [`Deal::deal_tag`].
    deal_tag: String,
}

/// Register the payee, quote the payout, and hand back the policy to seal.
async fn quote(State(app): State<Arc<App>>, Json(ask): Json<PayoutRequest>) -> Response {
    // Checked at the edge: every one of these ends up in a sealed policy.
    if ask.account_number.len() != 10 || !ask.account_number.bytes().all(|b| b.is_ascii_digit()) {
        return fail(StatusCode::BAD_REQUEST, "a Nigerian account number is 10 digits");
    }
    if !(16..=64).contains(&ask.deal_tag.len())
        || !ask
            .deal_tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return fail(
            StatusCode::BAD_REQUEST,
            "a deal tag is 16 to 64 letters, digits, '-' or '_'",
        );
    }
    if ask.amount_kobo <= 0 {
        return fail(StatusCode::BAD_REQUEST, "an amount must be more than nothing");
    }
    if ask.full_name.trim().is_empty() || ask.bank_name.trim().is_empty() {
        return fail(StatusCode::BAD_REQUEST, "a payee needs a name and a bank");
    }
    let escrow_key = ask.escrow_key.to_ascii_lowercase();
    if !app.wire.service.store.lock().await.shares.contains_key(&escrow_key) {
        return fail(
            StatusCode::BAD_REQUEST,
            "this platform holds no share of that escrow, so it is not paired into it",
        );
    }

    let account = match app
        .grid
        .external_account(&ask.account_number, &ask.bank_name, &ask.full_name)
        .await
    {
        Ok(account) => account,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, &e),
    };
    let quote = match app.grid.quote(&account.id, ask.amount_kobo, &ask.deal_tag).await {
        Ok(quote) => quote,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, &e),
    };
    // Both ids end up in paths the cosigner fetches, so they are held to the cosigner's own rule.
    if safe_reference(&account.id).is_none() || safe_reference(&quote.transaction_id).is_none() {
        return fail(
            StatusCode::BAD_GATEWAY,
            "Grid returned an id the cosigner could not look up",
        );
    }
    if quote.sending_currency.code != "USDB" {
        return fail(
            StatusCode::BAD_GATEWAY,
            &format!("Grid quoted in {}, not USDB", quote.sending_currency.code),
        );
    }
    let Some(price) = price_sats(quote.total_sending_amount, app.sats_per_usd) else {
        return fail(StatusCode::BAD_GATEWAY, "Grid quoted an amount that cannot be priced");
    };

    let policy = policy_for(&Deal {
        platform_script_hex: &app.payout_script,
        account_id: &account.id,
        account_number: &ask.account_number,
        bank_name: &ask.bank_name,
        deal_tag: &ask.deal_tag,
        amount_kobo: ask.amount_kobo,
        price_sats: price,
    });

    let request_id = app.wire.service.next_request_id().await;
    app.wire.service.store.lock().await.reimbursements.insert(
        request_id.clone(),
        Reimbursement {
            request_id: request_id.clone(),
            escrow_key,
            started_ref: quote.id.clone(),
            settled_ref: None,
            amount_minor: ask.amount_kobo as u64, // positive, checked above
            currency: "NGN".into(),
            sats: price,
            // Quoted, not paid for. It is `Started` once funded — see `fund`.
            stage: Stage::EscrowActive,
            last_refusal: None,
            needs_reconciliation: false,
            proposal: None,
            signatures: Vec::new(),
            expected_txid: None,
            ark_txid: None,
            given_up: false,
        },
    );
    if let Err(e) = app.wire.service.persist().await {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("the payout could not be written down: {e}"),
        );
    }

    Json(json!({
        "request_id": request_id,
        "quote_id": quote.id,
        "transaction_id": quote.transaction_id,
        "external_account_id": account.id,
        // The price. What Grid charges the platform is shown for the walkthrough, not agreed to.
        "sats": price,
        "grid_cost_micro_usdb": quote.total_sending_amount,
        "sats_per_usd": app.sats_per_usd,
        "amount_kobo": ask.amount_kobo,
        "expires_at": quote.expires_at,
        "policy": policy,
    }))
    .into_response()
}

/// The app has sealed the policy. Check this platform will be reimbursed, then pay.
async fn fund(State(app): State<Arc<App>>, Path(request_id): Path<String>) -> Response {
    if !app.funding.lock().unwrap().insert(request_id.clone()) {
        return fail(StatusCode::CONFLICT, "this payout is being funded already");
    }
    let _done = Funding(&app, request_id.clone());

    let Some(r) = app
        .wire
        .service
        .store
        .lock()
        .await
        .reimbursements
        .get(&request_id)
        .cloned()
    else {
        return fail(StatusCode::NOT_FOUND, "no payout is tracked under that id");
    };
    if r.given_up {
        return fail(
            StatusCode::CONFLICT,
            &format!(
                "this payout was given up on: {}",
                r.last_refusal.unwrap_or_default()
            ),
        );
    }
    if r.stage != Stage::EscrowActive {
        return fail(StatusCode::CONFLICT, "this payout has been funded already");
    }
    let quote = match app.grid.quote_status(&r.started_ref).await {
        Ok(quote) => quote,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, &e),
    };
    if quote.status != "PENDING" {
        return fail(
            StatusCode::CONFLICT,
            &format!("the quote is {}, not waiting to be paid for", quote.status),
        );
    }

    // Will this platform be reimbursed? Asked before paying, and the only refusal that means yes
    // is that the payout has not completed yet. The proposal this writes down is the one the real
    // ask reuses; if this goes no further, `watch` gives it up when the quote expires.
    match reimburse::ask_against(&app.wire, &request_id, Some(quote.transaction_id.as_str())).await {
        Asked::Refused { reason } if reason == PREFLIGHT_REFUSAL => {}
        Asked::Refused { reason } | Asked::Failed { reason } => {
            return fail(
                StatusCode::CONFLICT,
                &format!("not funded: the cosigner would not reimburse it — {reason}"),
            )
        }
        Asked::NeedsReconciliation => {
            return fail(
                StatusCode::CONFLICT,
                "not funded: this escrow needs reconciling first",
            )
        }
        // Only a completed payout can be paid for, and this one was PENDING a moment ago.
        Asked::Confirmed { ark_txid, sats } => {
            return Json(json!({ "outcome": "reimbursed", "ark_txid": ark_txid, "sats": sats }))
                .into_response()
        }
    }

    if let Err(e) = app.grid.sandbox_fund(&quote.id).await {
        // It may have gone through anyway. `watch` settles it from the quote's status either way.
        return fail(
            StatusCode::BAD_GATEWAY,
            &format!("funding may not have gone through: {e}"),
        );
    }
    app.wire.service.advance(&request_id, Stage::Started).await;
    if let Err(e) = app.wire.service.persist().await {
        // Recoverable: `watch` also follows a pre-flighted payout still marked as quoted.
        tracing::warn!(%request_id, %e, "funded, but could not write that down");
    }
    Json(json!({
        "outcome": "funded",
        "request_id": request_id,
        "transaction_id": quote.transaction_id,
    }))
    .into_response()
}

/// Releases a payout's place in [`App::funding`], however `fund` returns.
struct Funding<'a>(&'a App, String);

impl Drop for Funding<'_> {
    fn drop(&mut self) {
        self.0.funding.lock().unwrap().remove(&self.1);
    }
}

/// Ask Grid about every payout paid for, or pre-flighted and perhaps paid for.
///
/// A completed payout is asked to be reimbursed at once; the retry loop is the backup. A failed or
/// expired one is given up on, which frees its escrow for the next. Nothing here is remembered
/// between turns, so a restart picks up where it left off.
async fn watch(app: Arc<App>) {
    loop {
        tokio::time::sleep(WATCH_EVERY).await;
        for r in app.wire.service.tracked().await {
            let in_flight = r.stage == Stage::Started
                || (r.stage == Stage::EscrowActive && r.proposal.is_some());
            if r.given_up || !in_flight {
                continue;
            }
            let quote = match app.grid.quote_status(&r.started_ref).await {
                Ok(quote) => quote,
                Err(e) => {
                    tracing::debug!(request_id = %r.request_id, %e, "Grid did not answer");
                    continue;
                }
            };
            match quote.status.as_str() {
                "COMPLETED" => settle(&app, &r.request_id, &quote.transaction_id).await,
                "FAILED" | "EXPIRED" => {
                    let why = format!("the payout {}", quote.status.to_ascii_lowercase());
                    match app.wire.service.give_up(&r.request_id, &why).await {
                        Ok(()) => tracing::info!(request_id = %r.request_id, %why, "given up"),
                        Err(e) => tracing::warn!(request_id = %r.request_id, %e, "not given up yet"),
                    }
                }
                _ => {}
            }
        }
    }
}

/// The payout completed: name it, and ask to be reimbursed for it.
async fn settle(app: &App, request_id: &str, transaction_id: &str) {
    if let Some(r) = app
        .wire
        .service
        .store
        .lock()
        .await
        .reimbursements
        .get_mut(request_id)
    {
        r.settled_ref = Some(transaction_id.to_string());
    }
    app.wire.service.advance(request_id, Stage::Settled).await;
    if let Err(e) = app.wire.service.persist().await {
        tracing::warn!(%request_id, %e, "settled, but could not write that down");
    }
    match reimburse::ask(&app.wire, request_id).await {
        Asked::Confirmed { ark_txid, sats } => {
            tracing::info!(%request_id, %ark_txid, sats, "reimbursed")
        }
        other => tracing::info!(%request_id, ?other, "not reimbursed yet; asked again later"),
    }
}

fn fail(status: StatusCode, why: &str) -> Response {
    (status, Json(json!({ "error": why }))).into_response()
}
