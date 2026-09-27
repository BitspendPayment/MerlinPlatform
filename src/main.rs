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

mod deals;
mod grid;
mod payout;
mod treasury;

use std::collections::BTreeSet;
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ark::client::AspClient;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use cosigner::evidence::safe_reference;
use escrow_service::reimburse::{self, Asked};
use escrow_service::wire::{router as wire_router, Connections, Wire};
use escrow_service::{Reimbursement, Service, Stage};
use serde::Deserialize;
use serde_json::json;
use threshold::identifier::Identifier;

use deals::{Deals, Replay};
use grid::{Grid, GridError};
use payout::{policy_for, price_sats, Deal, PREFLIGHT_REFUSAL};
use treasury::Treasury;

/// How often [`watch`] asks Grid about payouts in flight.
const WATCH_EVERY: Duration = Duration::from_secs(5);

/// The least time a sealed deal must have left for the platform to pay into it. A sandbox payout
/// was repaid about a minute after funding; the rest is margin for a slow bank.
const MIN_DEAL_LEFT_SECS: u64 = 5 * 60;

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
    /// The platform's own key, where it is repaid: created on first run, 0600. Defaults to a file
    /// next to the store (`platform-state.json` → `platform-state.payout.key`).
    #[arg(long, env = "PLATFORM_PAYOUT_KEY")]
    payout_key: Option<std::path::PathBuf>,
    /// Where the operator routes listen — the treasury moves the platform's money, so this is
    /// loopback unless deliberately exposed.
    #[arg(long, env = "PLATFORM_OPERATOR_BIND", default_value = "127.0.0.1:7201")]
    operator_bind: String,
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
    #[arg(long, env = "PLATFORM_SATS_PER_USD", value_parser = clap::value_parser!(u64).range(1..))]
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
    /// Every deal tag quoted so far — see [`deals`].
    deals: Deals,
    /// Held while quoting, so two copies of one request cannot both miss [`App::deals`] and both
    /// quote.
    ///
    /// ponytail: one lock for every payout; per-tag locks if quoting ever needs to run in parallel.
    quoting: tokio::sync::Mutex<()>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    // Where this platform is paid: its own key's address. The sealed policy names that script as
    // the only place a release may go, and the treasury keeps what arrives there alive.
    let key_path = args
        .payout_key
        .clone()
        .or_else(|| args.store.as_ref().map(|s| s.with_extension("payout.key")))
        .ok_or_else(|| anyhow::anyhow!("--payout-key is needed when there is no --store"))?;
    let treasury = Arc::new(Treasury::open(&key_path, &args.asp).await?);
    let payout = treasury.address.clone();
    let payout_script = treasury.script.clone();

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
        grid: Grid::new(args.grid_client_id, args.grid_client_secret)?,
        payout_script,
        sats_per_usd: args.sats_per_usd,
        funding: Default::default(),
        // Next to the escrow store: `platform-state.json` keeps its tags in `platform-state.deals.json`.
        deals: Deals::load(args.store.as_ref().map(|p| p.with_extension("deals.json"))).await?,
        quoting: Default::default(),
    });
    tokio::spawn(watch(Arc::clone(&app)));
    tokio::spawn(treasury::keep_alive(Arc::clone(&treasury)));

    let router = wire_router(Arc::clone(&service), Arc::clone(&wire.connections)).merge(
        Router::new()
            .route("/payouts", post(quote))
            .route("/payouts/{request_id}/fund", post(fund))
            .with_state(app),
    );
    // The treasury moves the platform's own money, so it is never on the public listener.
    let operator = Router::new()
        .route("/treasury", get(treasury_view))
        .route("/treasury/send", post(treasury_send))
        .route("/treasury/renew", post(treasury_renew))
        .route("/treasury/exit", post(treasury_exit))
        .with_state(treasury);
    let listener = tokio::net::TcpListener::bind((args.bind.as_str(), args.port)).await?;
    let operator_listener = tokio::net::TcpListener::bind(&args.operator_bind).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        operator = %operator_listener.local_addr()?,
        identifier = %hex::encode(identifier.serialize()),
        payout = %payout,
        "platform up — Grid SANDBOX payouts, real Bitcoin escrow"
    );
    tokio::try_join!(
        axum::serve(listener, router).into_future(),
        axum::serve(operator_listener, operator).into_future(),
    )?;
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

    let _quoting = app.quoting.lock().await;
    let asked = deals::Asked {
        escrow_key: escrow_key.clone(),
        account_number: ask.account_number.clone(),
        bank_name: ask.bank_name.clone(),
        full_name: ask.full_name.clone(),
        amount_kobo: ask.amount_kobo,
    };
    match app.deals.replay(&ask.deal_tag, &asked).await {
        Replay::New => {}
        Replay::Same(answer) => return Json(answer).into_response(),
        Replay::Conflict => {
            return fail(
                StatusCode::CONFLICT,
                "that deal tag is already another payout's; a new payout needs a new tag",
            )
        }
    }

    // Keyed by the tag, so a request replayed after a crash, before its answer was written down,
    // gets Grid's first account and quote back rather than new ones.
    let account = match app
        .grid
        .external_account(
            &format!("merlin-payee-{}", ask.deal_tag),
            &ask.account_number,
            &ask.bank_name,
            &ask.full_name,
        )
        .await
    {
        Ok(account) => account,
        Err(e) => return grid_failed(e),
    };
    // Grid checks the name against the bank's records. Money sent to the wrong person is the
    // mistake a customer cannot take back, so a clear mismatch stops here; anything short of a
    // match goes back to the app with the bank's name, for the customer to confirm.
    if account.beneficiary_verification_status.as_deref() == Some("NOT_MATCHED") {
        return fail(
            StatusCode::BAD_REQUEST,
            "the bank says this account belongs to someone else; check the name and the number",
        );
    }
    let quote = match app
        .grid
        .quote(
            &format!("merlin-quote-{}", ask.deal_tag),
            &account.id,
            ask.amount_kobo,
            &ask.deal_tag,
        )
        .await
    {
        Ok(quote) => quote,
        Err(e) => return grid_failed(e),
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

    let answer = json!({
        "request_id": request_id,
        "quote_id": quote.id,
        "transaction_id": quote.transaction_id,
        "external_account_id": account.id,
        // Who the bank says the account belongs to, for the customer to confirm before sealing.
        "payee": {
            "name_given": ask.full_name,
            "name_at_bank": account.beneficiary_verified_data.and_then(|d| d.full_name),
            "name_check": account.beneficiary_verification_status,
        },
        // The price. What Grid charges the platform is shown for the walkthrough, not agreed to.
        "sats": price,
        "grid_cost_micro_usdb": quote.total_sending_amount,
        "sats_per_usd": app.sats_per_usd,
        "amount_kobo": ask.amount_kobo,
        "expires_at": quote.expires_at,
        "policy": policy,
    });
    if let Err(e) = app.deals.record(&ask.deal_tag, asked, answer.clone()).await {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("the payout could not be written down: {e}"),
        );
    }
    Json(answer).into_response()
}

#[derive(Deserialize)]
struct FundRequest {
    /// When the deal the app sealed ends, in seconds since the epoch.
    deal_deadline: u64,
}

/// Whether something ending at `until` leaves the platform time to be repaid before it does.
///
/// Two things end: the deal, and the escrow's VTXOs — a release spends them, so it has to happen
/// before the Ark server can sweep them.
fn lasts(what: &str, until: u64, now: u64) -> Result<(), String> {
    let left = until.saturating_sub(now);
    if left >= MIN_DEAL_LEFT_SECS {
        Ok(())
    } else {
        Err(format!(
            "{what} in {left}s, and a payout needs at least {MIN_DEAL_LEFT_SECS}s to be repaid in"
        ))
    }
}

/// When the escrow funds this payout's pre-flight picked out will expire, and whether that is too
/// soon to be repaid from them.
async fn escrow_lasts(app: &App, request_id: &str, now: u64) -> Result<(), String> {
    let outpoints: Vec<String> = app
        .wire
        .service
        .store
        .lock()
        .await
        .reimbursements
        .get(request_id)
        .and_then(|r| r.proposal.as_ref())
        .map(|p| p.inputs.iter().map(|i| format!("{}:{}", i.txid, i.vout)).collect())
        .unwrap_or_default();
    if outpoints.is_empty() {
        return Err("the pre-flight picked out no escrow funds".into());
    }
    let mut asp = AspClient::connect(&app.wire.service.asp_url)
        .await
        .map_err(|e| format!("connecting to the ASP: {e}"))?;
    let soonest = asp
        .get_vtxos_by_outpoints(&outpoints)
        .await
        .map_err(|e| format!("asking the indexer about the escrow's funds: {e}"))?
        .iter()
        .map(|v| v.expires_at)
        .min()
        .ok_or("the indexer does not know the escrow's funds")?;
    lasts("the escrow's funds expire", soonest.max(0) as u64, now)
}

/// The app has sealed the policy. Check this platform will be reimbursed, then pay.
async fn fund(
    State(app): State<Arc<App>>,
    Path(request_id): Path<String>,
    Json(ask): Json<FundRequest>,
) -> Response {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Taken on the app's word: the platform cannot see the sealed session, so this catches an
    // app's mistake, not its malice — the pre-flight proves a deal is live, not how long it lives.
    // ponytail: the real fix is the cosigner telling the service the sealed deadline.
    if let Err(why) = lasts("the deal ends", ask.deal_deadline, now) {
        return fail(StatusCode::CONFLICT, &format!("{why}; seal a longer one"));
    }
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
        Err(e) => return grid_failed(e),
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

    // The escrow's own clock. The release spends the VTXOs the pre-flight just picked out, and the
    // Ark server sweeps a VTXO once it expires — so funds about to expire are no funds to be
    // repaid from. Given up now, nothing signed and nothing paid, which also frees the escrow.
    if let Err(why) = escrow_lasts(&app, &request_id, now).await {
        if let Err(e) = app.wire.service.give_up(&request_id, &why).await {
            tracing::warn!(%request_id, %e, "not funded, and not given up yet");
        }
        return fail(StatusCode::CONFLICT, &format!("not funded: {why}"));
    }

    // Written down BEFORE paying. A crash, or a retry, after this point finds the payout funded and
    // pays nothing more. If the payment below then never happens, the quote expires and `watch`
    // gives the payout up — a payout paid for twice is the one outcome with no way back.
    app.wire.service.advance(&request_id, Stage::Started).await;
    if let Err(e) = app.wire.service.persist().await {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("not funded: could not write down that it was about to be: {e}"),
        );
    }
    if let Err(e) = app
        .grid
        .sandbox_fund(&format!("merlin-fund-{}", quote.id), &quote.id)
        .await
    {
        // Paid or not, `watch` settles it from the quote: repaid if it completes, given up if it
        // expires.
        return fail(
            StatusCode::BAD_GATEWAY,
            &format!("funding may not have gone through: {e}"),
        );
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

/// What the platform holds, and when each VTXO renews and expires.
async fn treasury_view(State(treasury): State<Arc<Treasury>>) -> Response {
    let held = match treasury.held().await {
        Ok(held) => held,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, &e),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    Json(json!({
        "address": treasury.address,
        "balance_sats": held.iter().map(|v| v.amount_sats).sum::<u64>(),
        "vtxos": held.iter().map(|v| json!({
            "outpoint": format!("{}:{}", v.txid, v.vout),
            "amount_sats": v.amount_sats,
            "renews_in_secs": v.renews_at() - now,
            "expires_in_secs": v.expires_at - now,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct TreasurySend {
    to_ark_address: String,
    sats: u64,
}

/// Send sats out of the treasury, to any Ark address.
async fn treasury_send(
    State(treasury): State<Arc<Treasury>>,
    Json(send): Json<TreasurySend>,
) -> Response {
    match treasury.send(&send.to_ark_address, send.sats).await {
        Ok(ark_txid) => Json(json!({ "ark_txid": ark_txid, "sats": send.sats })).into_response(),
        Err(e) => fail(StatusCode::BAD_REQUEST, &e),
    }
}

#[derive(Deserialize)]
struct TreasuryExit {
    to_address: String,
    sats: u64,
}

/// Take sats out of Ark to a bitcoin address: a collaborative exit, paid on chain by the next
/// batch's commitment transaction.
async fn treasury_exit(
    State(treasury): State<Arc<Treasury>>,
    Json(exit): Json<TreasuryExit>,
) -> Response {
    match treasury.exit(&exit.to_address, exit.sats).await {
        Ok(commitment_txid) => Json(json!({
            "commitment_txid": commitment_txid,
            "sats": exit.sats,
            "to_address": exit.to_address,
        }))
        .into_response(),
        Err(e) => fail(StatusCode::BAD_REQUEST, &e),
    }
}

/// Renew everything now, rather than when it falls due.
async fn treasury_renew(State(treasury): State<Arc<Treasury>>) -> Response {
    match treasury.renew(true).await {
        Ok(Some(commitment_txid)) => Json(json!({ "commitment_txid": commitment_txid })).into_response(),
        Ok(None) => Json(json!({ "commitment_txid": null, "why": "the treasury holds nothing" })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, &e),
    }
}

/// A Grid failure, told to the app as whose problem it is.
fn grid_failed(e: GridError) -> Response {
    match e {
        GridError::Rejected(why) => fail(StatusCode::BAD_REQUEST, &why),
        GridError::Unavailable(why) => fail(StatusCode::BAD_GATEWAY, &why),
    }
}

fn fail(status: StatusCode, why: &str) -> Response {
    (status, Json(json!({ "error": why }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_payout_depends_on_must_outlive_the_time_it_takes() {
        let now = 1_800_000_000;
        assert!(lasts("the deal ends", now + MIN_DEAL_LEFT_SECS, now).is_ok());
        assert!(lasts("the deal ends", now + MIN_DEAL_LEFT_SECS - 1, now).is_err());
        // Something already past says so, rather than wrapping round.
        assert!(lasts("the escrow's funds expire", now - 60, now)
            .unwrap_err()
            .starts_with("the escrow's funds expire in 0s"));
    }
}
