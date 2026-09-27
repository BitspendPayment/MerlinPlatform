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
    --store ./platform-state.json      # creates platform-state.payout.key on first run

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

## How it guards itself

- **It never pays twice.** A payout is written down as funded *before* Grid is paid, and every
  write to Grid carries an idempotency key.
- **A repeated request is the same payout.** The app's deal tag names one payout: asking again
  returns the first answer, and a tag reused for anything else is refused. Tags are kept next to
  the store (`platform-state.json` → `platform-state.deals.json`).
- **It checks who it is paying.** Grid checks the name against the bank; a `NOT_MATCHED` payee is
  refused, and the name the bank holds goes back to the app for the customer to confirm.
- **It will not pay into a deal about to end.** `POST /payouts/{id}/fund` takes the deadline the
  app sealed (`{"deal_deadline": <unix seconds>}`) and refuses with under five minutes left.
- **Grid is never waited on for ever**: 5 s to connect, 30 s per call.

## The platform's own bitcoin

Every repayment lands as a VTXO at the platform's Ark address, derived from its own key
(`--payout-key`, or `platform-state.payout.key` next to the store: created on first run, 0600).

- **Kept alive.** A VTXO expires — about four hours on regtest, weeks on a real Ark server — and the
  Ark server then sweeps it. The treasury renews everything it holds, merged into one VTXO, once
  any of it has used half its life.
- **Sent on.** The operator port (`--operator-bind`, `127.0.0.1:7201` by default; never the public
  one) has `GET /treasury` (balance, each VTXO, when it renews and expires),
  `POST /treasury/send {"to_ark_address", "sats"}` (within Ark; spends what expires soonest,
  change comes back), `POST /treasury/exit {"to_address", "sats"}` (out of Ark to a bitcoin
  address — a collaborative exit, paid on chain by the next batch's commitment transaction) and
  `POST /treasury/renew` (now, rather than when due).
- **The escrow's clock too.** A release spends the customer's escrow VTXOs, so `/fund` gives the
  payout up — nothing signed, nothing paid — when those expire within five minutes.

## Not yet

- Authentication: `/payouts` and `/fund` are open, and `/status` lists every customer's escrow
  and payouts. The app has to sign in before this faces the internet.
- Real JIT funding: the sandbox simulates it; production pays the quote's Spark instructions in
  USDB from the platform's treasury.
- A real price: `--sats-per-usd` is one fixed rate; production prices from a BTC/USD feed. The
  platform carries the USDB/BTC rate between pricing and being repaid.
- Storage: one JSON file, holding the platform's half of every escrow key in the clear, and the
  payout key in another. Production wants a database, and both kinds of key in a key service.
- Lightning: leaving Ark by Lightning needs a swap provider that speaks Ark.
- Exit fees: an exit leaves nothing for the Ark server, as this regtest server asks; one that
  charges for on-chain outputs needs its fee estimated and taken from the exit.
- An emergency exit: no unilateral exit is pre-signed for the platform's VTXOs, so if the Ark
  server disappears the platform cannot yet take its bitcoin on-chain alone.
- Escrow renewal: nothing renews a customer's escrow VTXOs (MerlinWallet), so a deal cannot
  outlive them.
- Ark fees: the policy says `fee_max 0`, true of Ark sends today; a server that starts charging
  would refuse every release until that changes.
- Webhooks: the platform polls Grid.
- A payout Grid reports COMPLETED and a bank later returns: the escrow cannot claw it back.
- The deadline is the app's word. Only the cosigner can tell the platform what was sealed.
