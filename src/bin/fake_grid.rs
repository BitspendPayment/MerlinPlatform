//! The fake Lightspark Grid, as a server. See `merlin_platform::fake_grid`.
//!
//! ```text
//!   cargo run --bin fake_grid -- --transact platform:dev-transact --view enclave:dev-view
//! ```
//!
//! **Nothing here moves money.** Every record it serves carries `"simulated": true` and every reply
//! carries an `x-simulated-payments: true` header.

use clap::Parser;
use merlin_platform::fake_grid::{router, Config, FakeGrid};

#[derive(Parser)]
#[command(about = "A fake Lightspark Grid, for working without Grid's sandbox. Simulated payments only.")]
struct Args {
    #[arg(long, default_value_t = 7300)]
    port: u16,
    /// Bind address. All interfaces by default: the enclave reaches this from its own network.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// The platform's token, `id:secret`: it may register, quote and fund.
    #[arg(long, env = "FAKE_GRID_TRANSACT", hide_env_values = true)]
    transact: String,
    /// The enclave's token, `id:secret`: it may only read.
    #[arg(long, env = "FAKE_GRID_VIEW", hide_env_values = true)]
    view: String,
    /// How long a funded payout takes to complete.
    #[arg(long, default_value_t = 5)]
    complete_after_secs: u64,
    /// How long a quote waits to be funded before it expires.
    #[arg(long, default_value_t = 180)]
    quote_ttl_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    for token in [&args.transact, &args.view] {
        anyhow::ensure!(token.contains(':'), "a token is id:secret");
    }
    // One token that was both would read as TRANSACT, and the VIEW token could move money.
    anyhow::ensure!(args.transact != args.view, "the TRANSACT and VIEW tokens must differ");

    let fake = FakeGrid::new(Config {
        transact: args.transact,
        view: args.view,
        complete_after_secs: args.complete_after_secs,
        quote_ttl_secs: args.quote_ttl_secs,
    });
    let listener = tokio::net::TcpListener::bind((args.bind.as_str(), args.port)).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        "fake Grid up — SIMULATED PAYMENTS ONLY, nothing here moves money"
    );
    axum::serve(listener, router(fake)).await?;
    Ok(())
}
