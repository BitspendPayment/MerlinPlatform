//! The platform's own bitcoin: the key it is repaid to, what it holds, keeping that alive, and
//! sending it on.
//!
//! Every repayment arrives as a VTXO at the platform's Ark address. A VTXO expires — about four
//! hours on regtest, weeks on a real Ark server — and at expiry the Ark server sweeps it, which is
//! money the platform was owed, gone. So the treasury renews them before that happens, merging
//! them into one as it goes, and can send them on: to a treasury desk, an exchange, anywhere with
//! an Ark address.

use std::path::Path;
use std::time::Duration;

use ark::client::batch::{DelegateOutput, DelegateSettleSession, DelegateVtxoInput};
use ark::client::proto::IndexerVtxo;
use ark::client::send::{SendSession, SendVtxoInput};
use ark::client::{ArkInfo, AspClient};
use bitcoin::secp256k1::{Keypair, Message, Secp256k1, SecretKey};
use rand::RngCore;
use tokio::sync::Mutex;

/// How often [`keep_alive`] looks for VTXOs due for renewal.
const CHECK_EVERY: Duration = Duration::from_secs(60);

pub struct Treasury {
    keypair: Keypair,
    /// Where the platform is paid, and the script an output to it carries.
    pub address: String,
    pub script: String,
    asp_url: String,
    /// One wallet operation at a time. A renewal and a send that picked the same VTXOs would be a
    /// double spend — refused by the Ark server at best, and a stuck batch at worst.
    busy: Mutex<()>,
}

/// One VTXO the platform holds.
#[derive(Debug, Clone)]
pub struct Held {
    pub txid: String,
    pub vout: u32,
    pub amount_sats: u64,
    pub created_at: i64,
    pub expires_at: i64,
    is_swept: bool,
}

impl Held {
    /// When it is due for renewal: once half its life has gone.
    ///
    /// Half, because a renewal has to find a batch to join and the Ark server has to be up to run
    /// one; renewing at the last minute bets the money on both. Scales by itself from regtest's
    /// hours to a real server's weeks.
    pub fn renews_at(&self) -> i64 {
        self.created_at + (self.expires_at - self.created_at) / 2
    }

    fn from_indexer(v: IndexerVtxo) -> Option<Self> {
        let outpoint = v.outpoint?;
        Some(Self {
            txid: outpoint.txid,
            vout: outpoint.vout,
            amount_sats: v.amount,
            created_at: v.created_at,
            expires_at: v.expires_at,
            is_swept: v.is_swept,
        })
    }
}

impl Treasury {
    /// Open the treasury, creating its key on first use.
    ///
    /// ponytail: the key is a 0600 file next to the store. Production keeps it in a key service;
    /// every repayment the platform is ever owed goes to the address it derives.
    pub async fn open(key_path: &Path, asp_url: &str) -> anyhow::Result<Self> {
        let secp = Secp256k1::new();
        let secret = if key_path.exists() {
            let hex_text = tokio::fs::read_to_string(key_path).await?;
            SecretKey::from_slice(&hex::decode(hex_text.trim())?)?
        } else {
            let mut bytes = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            let secret = SecretKey::from_slice(&bytes)?;
            write_secret(key_path, &hex::encode(bytes)).await?;
            tracing::info!(path = %key_path.display(), "created the platform's payout key");
            secret
        };
        let keypair = Keypair::from_secret_key(&secp, &secret);

        let info = info(asp_url).await?;
        let network = ark::client::parse_network(&info.network).map_err(anyhow::Error::msg)?;
        let address = ark::client::ark_address(
            &keypair.x_only_public_key().0.to_string(),
            &info.signer_pubkey,
            info.unilateral_exit_delay as u32,
            network,
        )
        .map_err(anyhow::Error::msg)?;
        let script = ark::client::ark_address_script_pubkey_hex(&address).map_err(anyhow::Error::msg)?;
        Ok(Self {
            keypair,
            address,
            script,
            asp_url: asp_url.to_string(),
            busy: Mutex::new(()),
        })
    }

    fn owner_pk_hex(&self) -> String {
        self.keypair.x_only_public_key().0.to_string()
    }

    /// BIP-340 signatures over the sighashes a session asked for, in order.
    fn sign(&self, sighashes: &[[u8; 32]]) -> Vec<[u8; 64]> {
        let secp = Secp256k1::new();
        sighashes
            .iter()
            .map(|h| {
                secp.sign_schnorr_no_aux_rand(&Message::from_digest(*h), &self.keypair)
                    .serialize()
            })
            .collect()
    }

    /// What the platform holds and can spend, soonest to expire first.
    pub async fn held(&self) -> Result<Vec<Held>, String> {
        let mut asp = connect(&self.asp_url).await?;
        let mut held: Vec<Held> = asp
            .get_vtxos_by_scripts(std::slice::from_ref(&self.script))
            .await
            .map_err(|e| format!("asking the indexer what the platform holds: {e}"))?
            .into_iter()
            .filter_map(Held::from_indexer)
            .collect();
        held.sort_by_key(|v| v.expires_at);
        Ok(held)
    }

    /// Renew everything the platform holds into one fresh VTXO, if any of it is due — or now,
    /// if `force`. Returns the batch's commitment txid, or `None` when nothing was due.
    pub async fn renew(&self, force: bool) -> Result<Option<String>, String> {
        let _busy = self.busy.lock().await;
        let held = self.held().await?;
        let now = unix_now();
        if held.is_empty() || !(force || held.iter().any(|v| now >= v.renews_at())) {
            return Ok(None);
        }

        // Everything, not only what is due: one VTXO renewed and consolidated is simpler to hold
        // than many ageing at different rates, and the batch costs the same.
        let all: Vec<&Held> = held.iter().collect();
        let total = held.iter().map(|v| v.amount_sats).sum();
        let commitment_txid = self
            .settle(
                &all,
                vec![DelegateOutput {
                    address: self.address.clone(),
                    amount_sats: total,
                }],
            )
            .await?;
        tracing::info!(%commitment_txid, sats = total, vtxos = held.len(), "renewed the treasury");
        Ok(Some(commitment_txid))
    }

    /// Take `sats` out of Ark to a bitcoin address: a collaborative exit.
    ///
    /// It rides a batch, like a renewal, and is paid on chain by the batch's commitment
    /// transaction; the change comes back as a fresh VTXO. Spends what expires soonest.
    ///
    /// ponytail: no fee is left for the Ark server, which is right for this regtest server. One that
    /// charges for on-chain outputs needs the intent fee estimated first and taken from the exit.
    pub async fn exit(&self, to_bitcoin_address: &str, sats: u64) -> Result<String, String> {
        // An Ark address here would quietly become a transfer inside Ark, which is not what was
        // asked for; the network is checked when the output is built.
        to_bitcoin_address
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
            .map_err(|e| format!("{to_bitcoin_address:?} is not a bitcoin address: {e}"))?;
        let _busy = self.busy.lock().await;
        let held = self.held().await?;
        let (picked, total) = pick(&held, sats)?;
        let dust = self.info().await?.dust as u64;
        let change = exit_change(total, sats, dust)?;
        let mut outputs = vec![DelegateOutput {
            address: to_bitcoin_address.to_string(),
            amount_sats: sats,
        }];
        if change > 0 {
            outputs.push(DelegateOutput {
                address: self.address.clone(),
                amount_sats: change,
            });
        }
        let commitment_txid = self.settle(&picked, outputs).await?;
        tracing::info!(%commitment_txid, sats, to = %to_bitcoin_address, "exited from the treasury");
        Ok(commitment_txid)
    }

    /// Spend `inputs` into `outputs` in the next batch. Returns its commitment txid.
    async fn settle(
        &self,
        inputs: &[&Held],
        outputs: Vec<DelegateOutput>,
    ) -> Result<String, String> {
        let mut asp = connect(&self.asp_url).await?;
        let info = asp
            .get_info()
            .await
            .map_err(|e| format!("asking the ASP what it is: {e}"))?;
        let inputs: Vec<DelegateVtxoInput> = inputs
            .iter()
            .map(|v| DelegateVtxoInput {
                txid: v.txid.clone(),
                vout: v.vout,
                amount_sats: v.amount_sats,
                is_swept: v.is_swept,
                exit_delay: info.unilateral_exit_delay as u32,
            })
            .collect();
        // A fresh key for the batch's MuSig2 tree signing, so the payout key never enters one.
        let mut tree_secret = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut tree_secret);
        let (mut session, sighashes) = DelegateSettleSession::generate_delegate(
            &self.owner_pk_hex(),
            &info.signer_pubkey,
            &info.forfeit_pubkey,
            &hex::encode(tree_secret),
            &inputs,
            &outputs,
            &info.forfeit_address,
            info.dust as u64,
            &info.network,
            None,
        )?;
        session.sign_with_frost(self.sign(&sighashes))?;
        let (commitment_txid, _) = session.settle(&mut asp).await?;
        Ok(commitment_txid)
    }

    async fn info(&self) -> Result<ArkInfo, String> {
        info(&self.asp_url).await.map_err(|e| e.to_string())
    }

    /// Send `sats` to another Ark address; the change comes back to the platform.
    pub async fn send(&self, to_ark_address: &str, sats: u64) -> Result<String, String> {
        let _busy = self.busy.lock().await;
        let held = self.held().await?;
        let (inputs, picked) = pick(&held, sats)?;
        let mut asp = connect(&self.asp_url).await?;
        let info = asp
            .get_info()
            .await
            .map_err(|e| format!("asking the ASP what it is: {e}"))?;
        let inputs: Vec<SendVtxoInput> = inputs
            .into_iter()
            .map(|v| SendVtxoInput {
                txid: v.txid.clone(),
                vout: v.vout,
                amount_sats: v.amount_sats,
                exit_delay: info.unilateral_exit_delay as u32,
            })
            .collect();
        let change = (picked > sats).then_some(self.address.as_str());
        let (mut session, sighashes) = SendSession::build(
            &self.owner_pk_hex(),
            &inputs,
            to_ark_address,
            sats,
            change,
            &info,
        )?;
        session.sign_with_frost(self.sign(&sighashes))?;
        let ark_txid = session.submit(&mut asp).await?;
        tracing::info!(%ark_txid, sats, to = %to_ark_address, "sent from the treasury");
        Ok(ark_txid)
    }
}

/// The VTXOs to spend for `sats`, soonest to expire first — spending those is spending the money
/// most at risk — and what they add up to.
fn pick(held: &[Held], sats: u64) -> Result<(Vec<&Held>, u64), String> {
    if sats == 0 {
        return Err("a send of nothing is not a send".into());
    }
    let mut picked = Vec::new();
    let mut total = 0u64;
    for v in held {
        if total >= sats {
            break;
        }
        picked.push(v);
        total += v.amount_sats;
    }
    if total < sats {
        // Change from a send a moment ago is not spendable until it settles, so "right now".
        return Err(format!(
            "the treasury can spend {total} sats right now, not {sats} (change from a recent \
             send takes a moment to settle)"
        ));
    }
    Ok((picked, total))
}

/// The change an exit of `sats` out of `total` leaves, if every output can go on chain.
///
/// Both outputs of an exit are real bitcoin outputs, so neither may be dust: the exit itself, and
/// the change unless there is none.
fn exit_change(total: u64, sats: u64, dust: u64) -> Result<u64, String> {
    if sats < dust {
        return Err(format!("{sats} sats is below dust ({dust}); it cannot go on chain"));
    }
    let change = total - sats;
    if change > 0 && change < dust {
        return Err(format!(
            "that leaves {change} sats of change, below dust ({dust}); exit {total}, or leave at \
             least {dust}"
        ));
    }
    Ok(change)
}

/// Renew whatever is due, for as long as the process lives.
pub async fn keep_alive(treasury: std::sync::Arc<Treasury>) {
    loop {
        match treasury.renew(false).await {
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => tracing::warn!(%e, "renewing the treasury failed; trying again shortly"),
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

async fn connect(asp_url: &str) -> Result<AspClient, String> {
    AspClient::connect(asp_url)
        .await
        .map_err(|e| format!("connecting to the ASP: {e}"))
}

async fn info(asp_url: &str) -> anyhow::Result<ArkInfo> {
    connect(asp_url)
        .await
        .map_err(anyhow::Error::msg)?
        .get_info()
        .await
        .map_err(|e| anyhow::anyhow!("asking the ASP what it is: {e}"))
}

/// Write a secret readable by this user alone. Created 0600, not chmod-ed after, so it is never
/// readable by anyone else even for a moment.
async fn write_secret(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vtxo(amount_sats: u64, expires_at: i64) -> Held {
        Held {
            txid: "11".repeat(32),
            vout: 0,
            amount_sats,
            created_at: 0,
            expires_at,
            is_swept: false,
        }
    }

    #[test]
    fn a_vtxo_is_renewed_once_half_its_life_is_gone() {
        let regtest = Held {
            created_at: 1_000,
            ..vtxo(20_000, 1_000 + 15_360)
        };
        assert_eq!(regtest.renews_at(), 1_000 + 7_680);
    }

    /// Spend what expires soonest, and only as much as the send needs.
    #[test]
    fn a_send_spends_the_money_most_at_risk_first() {
        let held = [vtxo(20_000, 100), vtxo(23_000, 200), vtxo(50_000, 300)];
        let (picked, total) = pick(&held, 30_000).unwrap();
        assert_eq!(picked.iter().map(|v| v.expires_at).collect::<Vec<_>>(), [100, 200]);
        assert_eq!(total, 43_000);
        assert!(pick(&held, 93_001).unwrap_err().contains("can spend 93000 sats"));
        assert!(pick(&held, 0).is_err());
    }

    /// Neither the exit nor its change may be dust; no change at all is fine.
    #[test]
    fn an_exit_puts_nothing_on_chain_that_is_dust() {
        assert_eq!(exit_change(23_038, 10_000, 330), Ok(13_038));
        assert_eq!(exit_change(23_038, 23_038, 330), Ok(0));
        assert!(exit_change(23_038, 329, 330).is_err());
        assert!(exit_change(23_038, 22_800, 330).unwrap_err().contains("238 sats of change"));
    }
}
