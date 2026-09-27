# MerlinPlatform

Send money to a Nigerian bank account. The platform pays first — a Lightspark Grid payout, funded
just in time in USDB (Grid's dollar, on Spark) — and is repaid out of the customer's Merlin escrow at
the price in sats the customer agreed, but only once the cosigner in the enclave has fetched the
payout and the payee's account from Grid itself and found exactly what the customer sealed: the
naira arrived, at the account they typed, for this deal.

It is MerlinWallet's `examples/card-escrow` with a Grid payout where the card purchase was, and it
shares its escrow machinery through MerlinWallet's `crates/escrow-service`.

## Running it

It builds on its own: MerlinWallet's crates come from GitHub, pinned to one commit in `Cargo.toml`.
Running it end to end also needs a MerlinWallet checkout next to this one (`../MerlinWallet`, for
the regtest stack and the wallet side) and two Grid **sandbox** tokens from app.lightspark.com:

| token | permission | used by |
|---|---|---|
| `GRID_CLIENT_ID` / `GRID_CLIENT_SECRET` | TRANSACT | this platform, to quote and fund payouts |
| `GRID_VIEW_ID` / `GRID_VIEW_SECRET` | VIEW only | the enclave, to check them — it lands in the image, so never a TRANSACT token |

```bash
cd ../MerlinWallet && make regtest-ark                 # bitcoind, arkd — stop `veiled` first (:18443)

cd ../MerlinPlatform
GRID_CLIENT_ID=… GRID_CLIENT_SECRET=… cargo run -- --sats-per-usd 1000 \
    --payout-xonly <32-byte hex> --store ./platform-state.json

cd ../MerlinWallet/e2e
GRID_VIEW_ID=… GRID_VIEW_SECRET=… dart run bin/grid_walkthrough.dart   # boots the enclave itself
```

## Testing

```bash
cargo test
```

The sealed policy, judged by the cosigner's own evaluator against responses recorded in the Grid
sandbox (`tests/fixtures/`): a completed payout releases the agreed price; before funding, the only
refusal is the one the platform funds on; a failed payout, another deal's payout, another bank
account, short naira and a release above the price are refused.

## Not yet

- Real JIT funding: the sandbox simulates it; production pays the quote's Spark instructions in
  USDB from the platform's treasury.
- A real price: `--sats-per-usd` is one fixed rate; production prices from a BTC/USD feed. The
  platform carries the USDB/BTC rate between pricing and being repaid.
- Webhooks: the platform polls Grid.
- A payout Grid reports COMPLETED and a bank later returns: the escrow cannot claw it back.
- The platform cannot see the session deadline; a payout that completes after it is not repaid.
