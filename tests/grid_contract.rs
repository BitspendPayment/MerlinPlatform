//! The platform's own Grid client against the fake Grid, over HTTP, on every rail: what the client
//! sends is what the fake takes, and what the fake answers is what the client reads.

use std::future::IntoFuture;
use std::sync::Arc;

use merlin_platform::corridors::{self, Corridor, Rail};
use merlin_platform::fake_grid::{self, Config, FakeGrid};
use merlin_platform::grid::{self, Grid};

const COMPLETE_AFTER: u64 = 5;

/// A fake Grid on a port of its own, and the platform's client dialling it.
async fn fake_grid() -> (Arc<FakeGrid>, Grid) {
    let fake = FakeGrid::manual(Config {
        transact: "platform:transact-secret".into(),
        view: "enclave:view-secret".into(),
        complete_after_secs: COMPLETE_AFTER,
        quote_ttl_secs: 180,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, fake_grid::router(Arc::clone(&fake))).into_future());
    let client = Grid::new(&url, "platform".into(), "transact-secret".into()).unwrap();
    (fake, client)
}

/// Register a payee on `rail` whose number ends in `ending`, quote a payout to them and fund it,
/// as the platform does. The quote's id.
async fn pay(grid: &Grid, corridor: &'static Corridor, rail: &Rail, ending: &str) -> String {
    let on = format!("{}-{}-{ending}", corridor.country, rail.rail);
    let fields = fake_grid::example_fields(corridor, rail, ending);
    let key = |what: &str| format!("merlin-{what}-{on}");
    let account = grid
        .external_account(&key("payee"), corridor.currency, rail.account_type, &fields, "Ada Obi")
        .await
        .unwrap();
    assert_eq!(account.beneficiary_verification_status.as_deref(), Some("MATCHED"), "{on}");
    let quote = grid
        .quote(&key("quote"), &account.id, rail.min_minor, &format!("deal-{on}"))
        .await
        .unwrap();
    assert_eq!((quote.status.as_str(), quote.sending_currency.code.as_str()), ("PENDING", "USDB"));
    assert!(grid::unix_secs(&quote.expires_at).is_some(), "{on}: {}", quote.expires_at);
    grid.sandbox_fund(&key("fund"), &quote.id).await.unwrap();
    assert_eq!(grid.quote_status(&quote.id).await.unwrap().status, "PROCESSING", "{on}");
    quote.id
}

#[tokio::test]
async fn every_rail_pays_out() {
    let (fake, grid) = fake_grid().await;
    for (corridor, rail) in corridors::rails() {
        let quote = pay(&grid, corridor, rail, "789").await;
        fake.advance(COMPLETE_AFTER);
        let done = grid.quote_status(&quote).await.unwrap();
        assert_eq!(done.status, "COMPLETED", "{} {}", corridor.country, rail.rail);
    }
}

#[tokio::test]
async fn a_payee_ending_002_is_paid_and_fails() {
    let (fake, grid) = fake_grid().await;
    for (corridor, rail) in corridors::rails() {
        let quote = pay(&grid, corridor, rail, "002").await;
        fake.advance(COMPLETE_AFTER);
        let done = grid.quote_status(&quote).await.unwrap();
        assert_eq!(done.status, "FAILED", "{} {}", corridor.country, rail.rail);
    }
}

#[tokio::test]
async fn a_payee_ending_102_is_somebody_else() {
    let (_, grid) = fake_grid().await;
    for (corridor, rail) in corridors::rails() {
        let fields = fake_grid::example_fields(corridor, rail, "102");
        let key = format!("merlin-payee-{}-{}", corridor.country, rail.rail);
        let account = grid
            .external_account(&key, corridor.currency, rail.account_type, &fields, "Ada Obi")
            .await
            .unwrap();
        let check = account.beneficiary_verification_status.as_deref();
        assert_eq!(check, Some("NOT_MATCHED"), "{} {}", corridor.country, rail.rail);
    }
}

/// The bank lists the platform serves the app come through the client, and what the fake lists is
/// what it takes.
#[tokio::test]
async fn the_bank_lists_are_grids() {
    let (_, grid) = fake_grid().await;
    for (corridor, rail) in corridors::rails() {
        let banks = grid.discoveries(corridor.country, corridor.currency).await.unwrap();
        let fields = fake_grid::example_fields(corridor, rail, "789");
        if let Some(bank) = fields.get("bankName") {
            assert!(banks.iter().any(|b| &b.bank_name == bank), "{bank} is listed");
        }
    }
    // Asked again, from memory; and a list is only ever the country's own.
    let again = grid.discoveries("NG", "NGN").await.unwrap();
    assert!(again.iter().any(|b| b.bank_name == "OPay"));
    assert!(!again.iter().any(|b| b.bank_name == "Standard Bank (South Africa)"));
}
