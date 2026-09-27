//! Merlin Platform — send money to a bank account or a mobile-money wallet: Nigeria, Kenya, Ghana
//! and South Africa to begin with (`GET /corridors`).
//!
//! ```text
//!   GRID_CLIENT_ID=… GRID_CLIENT_SECRET=… cargo run -- --sats-per-usd 1000 \
//!       --asp http://127.0.0.1:7070 --store ./platform-state.json
//! ```
//!
//! It pays first and is reimbursed after — `examples/card-escrow` in MerlinWallet, with a Grid
//! payout where the card purchase was:
//!
//! 1. `POST /payouts` — the app names a corridor's rail, the payee's account in that rail's
//!    fields, an amount in minor units and its own deal tag. The platform registers the account
//!    with Grid, quotes the payout funded just in time in USDB, prices it in sats, and answers
//!    with the policy the app seals into the customer's escrow.
//! 2. `POST /payouts/{request_id}/fund` — the app has sealed it. The platform asks the cosigner to
//!    reimburse it *before* paying, expecting exactly one refusal: the payout has not completed.
//!    Only then does it fund the quote — quoted again first, if it expired while the customer
//!    sealed.
//! 3. [`watch`] polls Grid. When the payout completes the platform asks again, and the cosigner —
//!    having fetched the payout and the payee's account for itself — co-signs its reimbursement.
//!    `GET /payouts/{deal_tag}` tells the app where its payout has got to.
//!
//! The enclave-facing routes (`/escrow/stream`, `/escrow/send`, `/pair/wallet`, `/status`) are
//! `escrow-service`'s.

mod deals;
mod treasury;

use std::collections::{BTreeMap, BTreeSet};
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ark::client::AspClient;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use cosigner::evidence::safe_reference;
use cosigner::policy::policy_sha256;
use escrow_service::reimburse::{self, Asked};
use escrow_service::trust::EnclaveTrust;
use escrow_service::wire::{router as wire_router, Connections, Wire};
use escrow_service::{Reimbursement, Service, Stage};
use merlin_platform::corridors;
use merlin_platform::grid::{self, Grid, GridError};
use merlin_platform::payout::{policy_for, preflight_clears, price_sats, Deal, GRID_ORIGIN};
use serde::Deserialize;
use serde_json::json;
use threshold::identifier::Identifier;

use deals::{Deals, QuoteMade, Quoted, Replay};
use treasury::Treasury;

/// How often [`watch`] asks Grid about payouts in flight.
const WATCH_EVERY: Duration = Duration::from_secs(5);

/// The least time a sealed deal must have left for the platform to pay into it, by default. A
/// sandbox payout was repaid about a minute after funding; the rest is margin for a slow bank.
///
/// ponytail: minutes suit the sandbox and regtest. A real bank can take hours, and a payout that
/// completes after the deal ends is never repaid — size `--min-deal-left-secs` to the slowest
/// corridor's worst case before real money.
const MIN_DEAL_LEFT_SECS: u64 = 5 * 60;

/// How long a quote that was never paid for is kept after it expired, for `fund` to quote it again
/// should the customer come back. After that [`watch`] gives it up.
const UNPAID_KEPT_SECS: u64 = 15 * 60;

#[derive(Parser)]
#[command(about = "Send to a bank or mobile money: paid out through Lightspark Grid, reimbursed from a Bitcoin escrow.")]
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
    /// Print this platform's FROST identifier, in hex — what the enclave's image names it by — and
    /// exit. Nothing else is needed for it.
    #[arg(long)]
    print_identifier: bool,
    /// Where to keep what must survive a restart. Its half of every escrow key lives here.
    #[arg(long, env = "PLATFORM_STORE")]
    store: Option<std::path::PathBuf>,
    /// What the platform charges, in sats, per US dollar a payout costs it at Grid.
    ///
    /// ponytail: one fixed rate, for the sandbox. Production prices from a BTC/USD feed, with
    /// the platform's margin in it.
    #[arg(
        long,
        env = "PLATFORM_SATS_PER_USD",
        value_parser = clap::value_parser!(u64).range(1..),
        required_unless_present = "print_identifier"
    )]
    sats_per_usd: Option<u64>,
    /// How long the deal the app seals should last, in seconds. Told to the app with every quote.
    #[arg(
        long,
        env = "PLATFORM_DEAL_SECS",
        default_value_t = 1800,
        value_parser = clap::value_parser!(u64).range(MIN_DEAL_LEFT_SECS..)
    )]
    deal_secs: u64,
    /// Where the platform dials Grid: an origin, with no path. Grid itself, or a fake one
    /// (`cargo run --bin fake_grid`).
    #[arg(long, env = "GRID_URL", default_value = GRID_ORIGIN)]
    grid_url: String,
    /// Grid's origin as the ENCLAVE reaches it, sealed into every policy: exactly the image's
    /// `SERVICE_CREDENTIAL_ORIGIN_GRID`. Defaults to `--grid-url`'s origin. A fake Grid on this
    /// host is `http://192.168.127.254:<port>` from inside a dev enclave.
    #[arg(long, env = "GRID_ORIGIN_SEALED")]
    grid_origin_sealed: Option<String>,
    /// A pins file for an enclave this platform believes — `deployment.json`'s shape: `pcr0`,
    /// `pcr16`, and `trust_root` for an emulated enclave. Repeatable. Nothing the enclave sends is
    /// heard without its runtime's attestation checking out against one of these; a dev enclave's
    /// file is written after each boot and read again when it changes.
    #[arg(long = "enclave-pins", env = "ENCLAVE_PINS", required_unless_present = "print_identifier")]
    enclave_pins: Vec<std::path::PathBuf>,
    /// The least time, in seconds, a sealed deal must have left for the platform to pay into it.
    #[arg(long, env = "PLATFORM_MIN_DEAL_LEFT_SECS", default_value_t = MIN_DEAL_LEFT_SECS)]
    min_deal_left_secs: u64,
    /// A Grid token that can TRANSACT. Never the one in the enclave's image.
    #[arg(
        long,
        env = "GRID_CLIENT_ID",
        hide_env_values = true,
        required_unless_present = "print_identifier"
    )]
    grid_client_id: Option<String>,
    #[arg(
        long,
        env = "GRID_CLIENT_SECRET",
        hide_env_values = true,
        required_unless_present = "print_identifier"
    )]
    grid_client_secret: Option<String>,
}

struct App {
    wire: Arc<Wire>,
    grid: Grid,
    /// Grid's origin as the enclave reaches it: the provider every sealed policy names.
    grid_origin: String,
    /// Where this platform is paid, as the script an output pays.
    payout_script: String,
    sats_per_usd: u64,
    /// How long a deal should last: told to the app, which seals it.
    deal_secs: u64,
    /// The least a sealed deal must have left to be paid into.
    min_deal_left: u64,
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

    let identifier =
        Identifier::derive(args.label.as_bytes()).map_err(|e| anyhow::anyhow!("{e}"))?;
    if args.print_identifier {
        println!("{}", hex::encode(identifier.serialize()));
        return Ok(());
    }
    let sats_per_usd = args.sats_per_usd.context("--sats-per-usd is needed")?;
    let grid_client_id = args.grid_client_id.clone().context("GRID_CLIENT_ID is needed")?;
    let grid_client_secret =
        args.grid_client_secret.clone().context("GRID_CLIENT_SECRET is needed")?;
    anyhow::ensure!(
        args.deal_secs > args.min_deal_left_secs,
        "--deal-secs must be longer than --min-deal-left-secs, or no deal could ever be paid into"
    );
    let grid_url = origin(&args.grid_url)?;
    let grid_origin = match &args.grid_origin_sealed {
        Some(sealed) => {
            origin(sealed)?;
            sealed.trim_end_matches('/').to_string()
        }
        None => grid_url.origin().ascii_serialization(),
    };

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

    let service = Service::new(
        identifier.clone(),
        payout.clone(),
        args.asp.clone(),
        args.store.clone(),
    );
    service.restore().await?;
    let wire = Arc::new(Wire {
        service: Arc::clone(&service),
        connections: Arc::new(Connections::new(EnclaveTrust::from_files(args.enclave_pins.clone()))),
    });

    // Anything a previous run left owed, asked for again; then keep asking. See `reimburse`.
    for (request_id, outcome) in reimburse::resume(&wire).await {
        tracing::info!(%request_id, ?outcome, "picked up where a previous run left off");
    }
    tokio::spawn(reimburse::keep_trying(Arc::clone(&wire)));

    let app = Arc::new(App {
        wire: Arc::clone(&wire),
        grid: Grid::new(grid_url.as_str(), grid_client_id, grid_client_secret)?,
        grid_origin: grid_origin.clone(),
        payout_script,
        sats_per_usd,
        deal_secs: args.deal_secs,
        min_deal_left: args.min_deal_left_secs,
        funding: Default::default(),
        // Next to the escrow store: `platform-state.json` keeps its tags in `platform-state.deals.json`.
        deals: Deals::load(args.store.as_ref().map(|p| p.with_extension("deals.json"))).await?,
        quoting: Default::default(),
    });
    tokio::spawn(watch(Arc::clone(&app)));
    tokio::spawn(treasury::keep_alive(Arc::clone(&treasury)));

    let router = wire_router(Arc::clone(&service), Arc::clone(&wire.connections)).merge(
        Router::new()
            .route("/corridors", get(corridors_table))
            .route("/corridors/{country}/banks", get(banks))
            .route("/payouts", post(quote))
            .route("/payouts/{deal_tag}", get(payout_state))
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
        grid = %grid_url,
        grid_sealed = %grid_origin,
        "platform up — Grid SANDBOX payouts, real Bitcoin escrow"
    );
    tokio::try_join!(
        axum::serve(listener, router).into_future(),
        axum::serve(operator_listener, operator).into_future(),
    )?;
    Ok(())
}

/// An origin — a scheme, a host and a port, and nothing after — as a sealed policy's provider must
/// be, and as the Grid client is dialled.
fn origin(url: &str) -> anyhow::Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("{url:?} is not a URL"))?;
    let bare = parsed.path() == "/" && parsed.query().is_none() && parsed.fragment().is_none();
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https") && bare && parsed.username().is_empty(),
        "{url:?} is not an origin: a scheme, a host and a port, with no path"
    );
    Ok(parsed)
}

/// Where this platform pays out, and what each rail asks for: the app renders its forms from it.
async fn corridors_table() -> Response {
    Json(json!({ "countries": corridors::CORRIDORS })).into_response()
}

/// The names a bank-list field takes in a country, from Grid. The networks a rail offers by name —
/// Ghana's mobile money, M-PESA — are left out: they are not banks.
async fn banks(State(app): State<Arc<App>>, Path(country): Path<String>) -> Response {
    let Some(corridor) = corridors::country(&country) else {
        return fail(
            StatusCode::NOT_FOUND,
            &format!("this platform does not pay out in {country:?}"),
        );
    };
    let networks: Vec<&str> = corridor
        .rails
        .iter()
        .flat_map(|r| r.fields)
        .filter_map(|f| f.options)
        .flatten()
        .copied()
        .collect();
    match app.grid.discoveries(corridor.country, corridor.currency).await {
        Ok(mut banks) => {
            banks.retain(|b| !networks.contains(&b.bank_name.as_str()));
            Json(json!({
                "country": corridor.country,
                "currency": corridor.currency,
                "banks": banks,
            }))
            .into_response()
        }
        Err(e) => grid_failed(e),
    }
}

#[derive(Deserialize)]
struct PayoutRequest {
    escrow_key: String,
    /// A corridor, and one of its rails — see `GET /corridors`.
    country: String,
    rail: String,
    /// The payee's account, by the rail's field keys: Grid's names for them.
    fields: BTreeMap<String, String>,
    full_name: String,
    /// In the currency's minor units — kobo, cents, pesewas. An integer, so nothing is lost on the
    /// way in.
    amount_minor: i64,
    /// Chosen by the app, never by this platform — see [`Deal::deal_tag`].
    deal_tag: String,
}

/// Register the payee, quote the payout, and hand back the policy to seal.
async fn quote(State(app): State<Arc<App>>, Json(ask): Json<PayoutRequest>) -> Response {
    // Checked at the edge: every one of these ends up in a sealed policy.
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
    let (corridor, rail) = match corridors::validate(
        &ask.country,
        &ask.rail,
        &ask.fields,
        &ask.full_name,
        ask.amount_minor,
    ) {
        Ok(found) => found,
        Err(why) => return fail(StatusCode::BAD_REQUEST, &why),
    };
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
        country: ask.country.clone(),
        rail: ask.rail.clone(),
        fields: ask.fields.clone(),
        full_name: ask.full_name.clone(),
        amount_minor: ask.amount_minor,
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
            corridor.currency,
            rail.account_type,
            &ask.fields,
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
            "the account belongs to someone else, its provider says; check the name and the \
             account",
        );
    }
    // It ends up in a path the cosigner fetches, so it is held to the cosigner's own rule.
    if safe_reference(&account.id).is_none() {
        return fail(
            StatusCode::BAD_GATEWAY,
            "Grid returned an id the cosigner could not look up",
        );
    }
    let quote = match app
        .grid
        .quote(
            &format!("merlin-quote-{}", ask.deal_tag),
            &account.id,
            ask.amount_minor,
            &ask.deal_tag,
        )
        .await
    {
        Ok(quote) => quote,
        Err(e) => return grid_failed(e),
    };
    let (price, expires_at) = match priced(&quote, app.sats_per_usd) {
        Ok(priced) => priced,
        Err(why) => return fail(StatusCode::BAD_GATEWAY, &why),
    };

    let policy = policy_for(&Deal {
        platform_script_hex: &app.payout_script,
        grid_origin: &app.grid_origin,
        account_id: &account.id,
        payee: ask.fields.clone(),
        deal_tag: &ask.deal_tag,
        currency: corridor.currency,
        amount_minor: ask.amount_minor,
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
            amount_minor: ask.amount_minor as u64, // positive, checked above
            currency: corridor.currency.into(),
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
        "deal_tag": ask.deal_tag,
        "external_account_id": account.id,
        // Who the bank says the account belongs to, for the customer to confirm before sealing.
        "payee": {
            "name_given": ask.full_name,
            "name_at_bank": account.beneficiary_verified_data.and_then(|d| d.full_name),
            "name_check": account.beneficiary_verification_status,
        },
        "currency": corridor.currency,
        "amount_minor": ask.amount_minor,
        // The price the customer agrees to: all the sealed caps release.
        "sats": price,
        "expires_at": quote.expires_at,
        // How long the deal the app seals should last.
        "deal_seconds": app.deal_secs,
        "policy": policy,
    });
    let quoted = Quoted {
        asked,
        answer: answer.clone(),
        request_id,
        account_id: account.id,
        quotes: vec![QuoteMade {
            id: quote.id,
            transaction_id: quote.transaction_id,
            cost_micro_usdb: quote.total_sending_amount,
            expires_at,
        }],
        ended: false,
    };
    if let Err(e) = app.deals.record(&ask.deal_tag, quoted).await {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("the payout could not be written down: {e}"),
        );
    }
    Json(answer).into_response()
}

/// What a fresh quote comes to: its price in sats, and when it expires. Or why Grid's answer is
/// one this platform cannot quote on.
fn priced(quote: &grid::Quote, sats_per_usd: u64) -> Result<(u64, u64), String> {
    // The transaction is what the cosigner fetches, by id, so the id is held to its own rule.
    if safe_reference(&quote.transaction_id).is_none() {
        return Err("Grid returned an id the cosigner could not look up".into());
    }
    if quote.sending_currency.code != "USDB" {
        return Err(format!("Grid quoted in {}, not USDB", quote.sending_currency.code));
    }
    let price = price_sats(quote.total_sending_amount, sats_per_usd)
        .ok_or("Grid quoted an amount that cannot be priced")?;
    let expires_at = grid::unix_secs(&quote.expires_at)
        .ok_or_else(|| format!("Grid quoted an expiry this cannot read: {:?}", quote.expires_at))?;
    Ok((price, expires_at))
}

/// Where one payout has got to, asked by its deal tag — which only the app that chose it knows.
/// Nothing about anybody else's payout is here.
async fn payout_state(State(app): State<Arc<App>>, Path(deal_tag): Path<String>) -> Response {
    let Some(deal) = app.deals.get(&deal_tag).await else {
        return fail(StatusCode::NOT_FOUND, "no payout has that deal tag");
    };
    let tracked = app
        .wire
        .service
        .store
        .lock()
        .await
        .reimbursements
        .get(&deal.request_id)
        .cloned();
    let Some(r) = tracked else {
        return fail(StatusCode::NOT_FOUND, "no payout is tracked for that deal tag");
    };
    // Grid's word, if it answers; the platform's own record does not wait for it.
    let grid_status = app.grid.quote_status(&r.started_ref).await.ok().map(|q| q.status);
    let funding = app.funding.lock().unwrap().contains(&r.request_id);
    Json(json!({
        "state": state_of(&r, grid_status.as_deref(), funding),
        "grid_status": grid_status,
        "failure": if r.given_up { r.last_refusal.clone() } else { None },
        "sats": r.sats,
        "ark_txid": r.ark_txid,
        // Given up, and the cosigner has ended the deal: the escrow is the owner's again.
        "deal_ended": deal.ended,
    }))
    .into_response()
}

/// A payout's state as the app shows it, from the platform's stage and Grid's status.
fn state_of(r: &Reimbursement, grid_status: Option<&str>, funding: bool) -> &'static str {
    if r.given_up {
        return "failed";
    }
    match r.stage {
        Stage::Paired | Stage::EscrowActive if funding => "funding",
        Stage::Paired | Stage::EscrowActive => "quoted",
        // `watch` moves it on within a few seconds of Grid's word.
        Stage::Started => match grid_status {
            Some("COMPLETED") => "paid_out",
            Some("FAILED" | "EXPIRED") => "failed",
            _ => "paying",
        },
        Stage::Settled => "paid_out",
        Stage::EvidenceVerified | Stage::ReleaseSigned => "repaying",
        Stage::ReleaseConfirmed => "repaid",
    }
}

/// Whether something ending at `until` leaves the platform `min` seconds to be repaid before it
/// does.
///
/// The escrow's VTXOs end: a release spends them, so it has to happen before the Ark server can
/// sweep them. (The deal ends too, and the pre-flight checks that from the cosigner's own word —
/// see [`preflight_clears`].)
fn lasts(what: &str, until: u64, now: u64, min: u64) -> Result<(), String> {
    let left = until.saturating_sub(now);
    if left >= min {
        Ok(())
    } else {
        Err(format!(
            "{what} in {left}s, and a payout needs at least {min}s to be repaid in"
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
    lasts("the escrow's funds expire", soonest.max(0) as u64, now, app.min_deal_left)
}

/// The app has sealed the policy. Check this platform will be reimbursed, then pay.
async fn fund(
    State(app): State<Arc<App>>,
    Path(request_id): Path<String>,
    // Read and dropped: `/fund` takes no body, but a client may send one anyway (`{}`, say). Left
    // unread, it is still in the socket when the reply goes out, and closing a socket with unread
    // bytes resets it — taking the reply with it. The payout is funded and the app never hears so.
    _body: axum::body::Bytes,
) -> Response {
    let now = unix_now();
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
    let mut quote = match app.grid.quote_status(&r.started_ref).await {
        Ok(quote) => quote,
        Err(e) => return grid_failed(e),
    };
    // A quote lasts minutes, and sealing can take longer. One that expired unpaid is quoted again:
    // the same payee, amount and deal tag, which the sealed policy holds of just as well.
    let lapsed = grid::unix_secs(&quote.expires_at).is_some_and(|at| at <= now);
    if quote.status == "EXPIRED" || (quote.status == "PENDING" && lapsed) {
        quote = match requote(&app, &request_id, r.sats).await {
            Ok(quote) => quote,
            Err(refused) => return refused,
        };
    }
    if quote.status != "PENDING" {
        return fail(
            StatusCode::CONFLICT,
            &format!("the quote is {}, not waiting to be paid for", quote.status),
        );
    }

    // Will this platform be reimbursed? Asked before paying: the only refusal that means yes is
    // that the payout has not completed yet — about the deal this platform offered, with time left
    // to be repaid in, both in the cosigner's own words and not the app's. The proposal this writes
    // down is the one the real ask reuses; if this goes no further, `watch` gives it up when the
    // quote expires.
    let Some(offered) = app
        .deals
        .of_request(&request_id)
        .await
        .and_then(|(_, deal)| deal.offered_policy())
    else {
        return fail(StatusCode::CONFLICT, "not funded: there is no offered deal on record");
    };
    match reimburse::ask_against(&app.wire, &request_id, Some(quote.transaction_id.as_str())).await {
        Asked::Refused { reason, deal } => {
            if let Err(why) =
                preflight_clears(&reason, deal.as_ref(), &offered, now as i64, app.min_deal_left as i64)
            {
                return fail(StatusCode::CONFLICT, &format!("not funded: {why}"));
            }
        }
        Asked::Failed { reason } => {
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
        give_up(&app, &request_id, &why).await;
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

/// Quote a deal again, its quote having expired unpaid: the same payee, amount and deal tag, under
/// a new idempotency key — the first key would only replay the expired quote. Refused if it now
/// costs more than the price the customer agreed, which is all the sealed caps release.
///
/// The error is the reply `fund` sends, whole: boxing it would only be undone at the one caller.
#[allow(clippy::result_large_err)]
async fn requote(app: &App, request_id: &str, agreed_sats: u64) -> Result<grid::Quote, Response> {
    let Some((tag, deal)) = app.deals.of_request(request_id).await else {
        return Err(fail(
            StatusCode::CONFLICT,
            "the quote expired, and there is no deal on record to quote again",
        ));
    };
    let key = format!("merlin-quote-{tag}-r{}", deal.quotes.len());
    let quote = app
        .grid
        .quote(&key, &deal.account_id, deal.asked.amount_minor, &tag)
        .await
        .map_err(grid_failed)?;
    let (price, expires_at) =
        priced(&quote, app.sats_per_usd).map_err(|why| fail(StatusCode::BAD_GATEWAY, &why))?;
    if price > agreed_sats {
        return Err(fail(
            StatusCode::CONFLICT,
            &format!(
                "the quote expired, and the payout now costs {price} sats, more than the \
                 {agreed_sats} agreed; ask for a new payout"
            ),
        ));
    }
    let made = QuoteMade {
        id: quote.id.clone(),
        transaction_id: quote.transaction_id.clone(),
        cost_micro_usdb: quote.total_sending_amount,
        expires_at,
    };
    // Written down before it is used, like the first.
    let written = async {
        app.deals.requoted(&tag, made).await?;
        if let Some(r) = app.wire.service.store.lock().await.reimbursements.get_mut(request_id) {
            r.started_ref = quote.id.clone();
        }
        app.wire.service.persist().await
    };
    if let Err(e) = written.await {
        return Err(fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("not funded: the new quote could not be written down: {e}"),
        ));
    }
    tracing::info!(%request_id, quote = %quote.id, "quoted again: the last one expired unpaid");
    Ok(quote)
}

/// Releases a payout's place in [`App::funding`], however `fund` returns.
struct Funding<'a>(&'a App, String);

impl Drop for Funding<'_> {
    fn drop(&mut self) {
        self.0.funding.lock().unwrap().remove(&self.1);
    }
}

/// Ask Grid about every payout paid for, or pre-flighted and perhaps paid for; and give up on
/// quotes nobody paid for.
///
/// A completed payout is asked to be reimbursed at once; the retry loop is the backup. A failed or
/// expired one is given up on, which frees its escrow for the next. Nothing here is remembered
/// between turns, so a restart picks up where it left off.
async fn watch(app: Arc<App>) {
    loop {
        tokio::time::sleep(WATCH_EVERY).await;
        let now = unix_now();
        let expiries = app.deals.expiries().await;
        for r in app.wire.service.tracked().await {
            if r.given_up || app.funding.lock().unwrap().contains(&r.request_id) {
                continue;
            }
            // Quoted, and neither pre-flighted nor paid for: it holds nothing, and Grid need not
            // be asked. `fund` quotes it again if the customer comes back — for a while.
            if r.stage == Stage::EscrowActive && r.proposal.is_none() {
                let expired = expiries.get(&r.request_id);
                if expired.is_some_and(|at| now > at + UNPAID_KEPT_SECS) {
                    give_up(&app, &r.request_id, "the quote expired, and nobody paid for it").await;
                }
                continue;
            }
            let in_flight = matches!(r.stage, Stage::Started | Stage::EscrowActive);
            if !in_flight {
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
                    give_up(&app, &r.request_id, &why).await;
                }
                _ => {}
            }
        }
    }
}

/// Give a payout up, and end its deal so the customer's escrow is free at once rather than at the
/// deal's deadline. Nothing was signed for it — `give_up` refuses otherwise — so the platform is
/// owed nothing, and the deal protects only the platform.
///
/// Ending it waits on the enclave's connection, so it runs apart from the caller; a deal that is
/// not ended simply runs to its deadline.
async fn give_up(app: &Arc<App>, request_id: &str, why: &str) {
    if let Err(e) = app.wire.service.give_up(request_id, why).await {
        tracing::warn!(%request_id, %e, "not given up yet");
        return;
    }
    tracing::info!(%request_id, %why, "given up");
    let Some((tag, deal)) = app.deals.of_request(request_id).await else {
        return;
    };
    let Some(offered) = deal.offered_policy() else {
        return;
    };
    let (app, request_id) = (Arc::clone(app), request_id.to_string());
    tokio::spawn(async move {
        let hash = policy_sha256(&offered);
        match reimburse::end_deal(&app.wire, &deal.asked.escrow_key, &hash).await {
            Ok(()) => {
                if let Err(e) = app.deals.ended(&tag).await {
                    tracing::warn!(%request_id, %e, "the deal ended, but that was not written down");
                }
                tracing::info!(%request_id, "the deal is ended: the escrow is the customer's again");
            }
            // Never sealed, or sealed differently: nothing of ours holds the escrow.
            Err(why) => tracing::info!(%request_id, %why, "the deal was not ended"),
        }
    });
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
        let min = MIN_DEAL_LEFT_SECS;
        assert!(lasts("the escrow's funds expire", now + min, now, min).is_ok());
        assert!(lasts("the escrow's funds expire", now + min - 1, now, min).is_err());
        // Something already past says so, rather than wrapping round.
        assert!(lasts("the escrow's funds expire", now - 60, now, min)
            .unwrap_err()
            .starts_with("the escrow's funds expire in 0s"));
    }

    /// What the app is told, from the platform's stage and Grid's status.
    #[test]
    fn a_payouts_state_follows_its_stage_and_grids_word() {
        let at = |stage: Stage| Reimbursement {
            request_id: "reimb-0001".into(),
            escrow_key: "02aa".into(),
            started_ref: "Quote:1".into(),
            settled_ref: None,
            amount_minor: 3_000_000,
            currency: "NGN".into(),
            sats: 23_006,
            stage,
            last_refusal: None,
            needs_reconciliation: false,
            proposal: None,
            signatures: Vec::new(),
            expected_txid: None,
            ark_txid: None,
            given_up: false,
        };
        assert_eq!(state_of(&at(Stage::EscrowActive), Some("PENDING"), false), "quoted");
        assert_eq!(state_of(&at(Stage::EscrowActive), Some("PENDING"), true), "funding");
        assert_eq!(state_of(&at(Stage::Started), Some("PROCESSING"), false), "paying");
        assert_eq!(state_of(&at(Stage::Started), None, false), "paying");
        assert_eq!(state_of(&at(Stage::Started), Some("COMPLETED"), false), "paid_out");
        assert_eq!(state_of(&at(Stage::Started), Some("FAILED"), false), "failed");
        assert_eq!(state_of(&at(Stage::Settled), Some("COMPLETED"), false), "paid_out");
        assert_eq!(state_of(&at(Stage::ReleaseSigned), Some("COMPLETED"), false), "repaying");
        assert_eq!(state_of(&at(Stage::ReleaseConfirmed), Some("COMPLETED"), false), "repaid");
        let given_up = Reimbursement { given_up: true, ..at(Stage::EscrowActive) };
        assert_eq!(state_of(&given_up, Some("EXPIRED"), false), "failed");
    }

    /// Grid is dialled, and sealed, as an origin: never with a path, which the cosigner refuses.
    #[test]
    fn grid_is_an_origin() {
        assert!(origin("https://api.lightspark.com").is_ok());
        assert!(origin("http://192.168.127.254:7300/").is_ok());
        assert_eq!(
            origin("https://api.lightspark.com:443").unwrap().origin().ascii_serialization(),
            "https://api.lightspark.com"
        );
        assert!(origin("https://api.lightspark.com/grid/2025-10-13").is_err());
        assert!(origin("https://api.lightspark.com?x=1").is_err());
        assert!(origin("ftp://api.lightspark.com").is_err());
        assert!(origin("api.lightspark.com").is_err());
    }
}
