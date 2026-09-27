# MerlinPlatform

Send money to a bank account or a mobile-money wallet: Nigeria (bank), Kenya (M-PESA), Ghana (bank
and mobile money) and South Africa (bank) to begin with. The platform pays first — a Lightspark Grid
payout, funded just in time in USDB (Grid's dollar, on Spark) — and is repaid out of the customer's
Merlin escrow at the price in sats the customer agreed, but only once the cosigner in the enclave
has fetched the payout and the payee's account from Grid itself and found exactly what the customer
sealed: the money arrived, at the account they typed, for this deal.

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

On regtest none of that is needed: MerlinWallet's stack runs this platform against the fake Grid
below, built from this checkout against MerlinWallet's own crates.

```bash
cd ../MerlinWallet
make up                  # regtest, arkd, this platform + the fake Grid, and a dev enclave (for the app)
make send-walkthrough    # or: every corridor end to end, with an enclave of its own
```

Against Grid's sandbox:

```bash
GRID_CLIENT_ID=… GRID_CLIENT_SECRET=… cargo run -- --sats-per-usd 1000 \
    --store ./platform-state.json \
    --enclave-pins ./deployment.json   # the enclave to believe; the payout key is made on first run
```

A `platform-state.deals.json` written before the corridors does not load: delete it.

**It believes nothing the enclave says without its attestation.** The enclave's runtime signs every
stream it opens here and every message it sends with an attestation document binding the
connection and the exact bytes; `--enclave-pins` (repeatable) names the enclaves to believe, in
`deployment.json`'s shape — `pcr0`, `pcr16`, and `trust_root` for an emulated one. A dev enclave's
file is written after each boot (`make up` does it) and read again whenever it changes.

Other flags: `--grid-url` (where Grid is dialled; `https://api.lightspark.com`),
`--grid-origin-sealed` (Grid's origin as the *enclave* reaches it, sealed into every policy —
exactly the image's `SERVICE_CREDENTIAL_ORIGIN_GRID`; defaults to `--grid-url`'s origin),
`--deal-secs` (how long the deal the app seals should last; 1800), `--min-deal-left-secs` (the
least a sealed deal must have left to be paid into; 300 — minutes suit regtest, a real bank needs
hours) and `--print-identifier` (print the platform's FROST identifier, hex, and exit — for dev
scripts; needs nothing else).

### Without Grid: the fake Grid

`cargo run --bin fake_grid -- --transact platform:dev-transact --view enclave:dev-view` serves the
part of Grid's API the platform and the cosigner use, on `:7300`, under `/grid/2025-10-13`, in
Grid's shapes (checked against the recordings in `tests/fixtures/`) — and moves no money: every
record says `"simulated": true` and every reply carries `x-simulated-payments: true`. Point the
platform at it with `--grid-url http://127.0.0.1:7300 --grid-origin-sealed
http://192.168.127.254:7300` (the host, as a dev enclave sees it), `GRID_CLIENT_ID=platform
GRID_CLIENT_SECRET=dev-transact`. The enclave's image then carries
`SERVICE_CREDENTIALS_GRID=enclave:dev-view` and `SERVICE_CREDENTIAL_ORIGIN_GRID=http://192.168.127.254:7300`,
with that origin in its egress.

Like Grid's sandbox, the payee's last three digits (account number, or phone number on mobile
money) decide what happens: `102` the name check says `NOT_MATCHED`, `103` `PARTIAL_MATCH`, `104`
`PENDING`, `105` refused, `106` `UNSUPPORTED`, `107` `CHECKED_BY_RECEIVING_FI`; once funded, `002`
fails (refunded), `003` completes ten times slower, `005` completes and is then returned, anything
else completes after `--complete-after-secs` (5). A quote unfunded after `--quote-ttl-secs` (180)
expires. It keeps everything in memory: a restart strands a payout in flight.

## The API

- `GET /corridors` — every country, its currency and decimals, its rails, and each rail's fields
  (Grid's key, a label, `text` with a `prefix` and `digits: {min, max}`, or `select` with inline
  `options` or `from_bank_list`) and amount limits. The app renders its forms from it. Every
  payout also names its payee, `full_name` (1 to 250 characters).
- `GET /corridors/{country}/banks` — the names a bank-list field takes, from Grid (cached).
- `POST /payouts {escrow_key, country, rail, fields: {<grid key>: value}, full_name, amount_minor,
  deal_tag}` → `{request_id, deal_tag, external_account_id, payee: {name_given, name_at_bank,
  name_check}, currency, amount_minor, sats, expires_at, deal_seconds, policy}`.
- `POST /payouts/{request_id}/fund` — the app has sealed the policy. No body: what was sealed, and
  until when, the platform hears from the cosigner.
- `GET /payouts/{deal_tag}` → `{state, grid_status, failure, sats, ark_txid, deal_ended}`, where
  `state` is `quoted`, `funding`, `paying`, `paid_out`, `repaying`, `repaid` or `failed`. Only that
  deal's: the tag the app chose is what lets it ask.

## Testing

```bash
cargo test
```

The sealed policy, judged by the cosigner's own evaluator against responses recorded in the Grid
sandbox (`tests/fixtures/`): a completed payout releases the agreed price; before funding, the only
refusal is the one the platform funds on; a failed payout, another deal's payout, another bank
account, short naira and a release above the price are refused. The same, on every rail, against
records the fake Grid made — synthetic, and labelled so. Also: each rail's field rules; the fake
Grid itself (its tokens, its checks, the endings, a payout's life on a clock the test moves, its
idempotency, its shapes against the recordings); and `tests/grid_contract.rs`, the platform's own
Grid client against the fake over HTTP, on every rail.

## How it guards itself

- **It never pays twice.** A payout is written down as funded *before* Grid is paid, and every
  write to Grid carries an idempotency key.
- **A repeated request is the same payout.** The app's deal tag names one payout: asking again
  returns the first answer, and a tag reused for anything else is refused. Tags are kept next to
  the store (`platform-state.json` → `platform-state.deals.json`).
- **It checks who it is paying.** Grid checks the name against the bank; a `NOT_MATCHED` payee is
  refused, and the name the bank holds goes back to the app for the customer to confirm.
- **It pays only into the deal it offered, with time to be repaid in.** Before paying, `/fund` asks
  the cosigner for its reimbursement and expects exactly one refusal — the payout has not completed
  — which comes with the sealed deal's terms: its deadline and the hash of its policy. It pays only
  if that hash is the offered policy's (a policy with a term appended after `status` refuses in the
  same words, and then for ever) and the deal has `--min-deal-left-secs` to run. Both come from the
  cosigner, never the app.
- **A payout given up frees the escrow.** When a payout fails or expires the platform gives it up
  — never once anything is signed — and tells the cosigner to end the deal, so the customer's
  escrow is free at once instead of at the deadline. `GET /payouts/{deal_tag}` says so
  (`deal_ended`).
- **A quote that lapsed while the customer sealed is quoted again**, by `/fund`: the same payee,
  amount and deal tag, under a new idempotency key — and only if it costs no more than the price
  agreed. One nobody pays for is given up fifteen minutes after it expired.
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
- The deadline is the app's word. Only the cosigner can tell the platform what was sealed; so
  `deal_ended` is always `false` for now.
- Other corridors on the real Grid: only naira has been paid out in the sandbox. Whether USDB
  funds the others, and whether Grid echoes a phone number back as sent (the policy pins it), is
  unverified; so are the spellings of Ghana's mobile-money networks, and Grid's own amount limits
  (the table's are placeholders, about $1 to $1,000).
