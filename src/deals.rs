//! Every deal tag this platform has quoted, what it answered, and the Grid quotes made for it.
//!
//! A tag is the app's name for one payout, and the sealed policy pins it. So a repeated request —
//! a retry over a flaky connection — must get the same payout back, not a second quote; and a tag
//! already quoted must never be quoted for anything else. A second payout under one tag is what
//! would let one payout satisfy two customers' deals.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// What makes two requests the same payout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asked {
    pub escrow_key: String,
    pub country: String,
    pub rail: String,
    /// The payee's account, by Grid's names for its fields.
    pub fields: BTreeMap<String, String>,
    pub full_name: String,
    pub amount_minor: i64,
}

/// One deal tag's payout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quoted {
    pub asked: Asked,
    /// Exactly what the app was sent, policy and all.
    pub answer: serde_json::Value,
    pub request_id: String,
    /// Grid's id for the payee's account: what every quote for this deal pays.
    pub account_id: String,
    /// Every quote made for it, the first first. One that expired unpaid is replaced — see `fund`.
    pub quotes: Vec<QuoteMade>,
    /// This platform gave the payout up and the cosigner has ended its deal, so the escrow is free
    /// at once rather than at the deal's deadline.
    #[serde(default)]
    pub ended: bool,
}

impl Quoted {
    /// The policy this platform offered for the deal: what the one sealed must be.
    pub fn offered_policy(&self) -> Option<cosigner::policy::Policy> {
        serde_json::from_value(self.answer.get("policy")?.clone()).ok()
    }
}

/// A Grid quote made for a deal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteMade {
    pub id: String,
    pub transaction_id: String,
    /// What Grid charges the platform for it, in micro-USDB.
    pub cost_micro_usdb: u64,
    /// When it expires unpaid, in seconds since the epoch.
    pub expires_at: u64,
}

/// What to do with a request, given what the tag has seen before.
#[derive(Debug, PartialEq)]
pub enum Replay {
    New,
    /// The same payout, asked again: send the first answer back.
    Same(serde_json::Value),
    /// The tag is already another payout's.
    Conflict,
}

pub struct Deals {
    tags: Mutex<BTreeMap<String, Quoted>>,
    path: Option<PathBuf>,
}

impl Deals {
    /// Read back what a previous run recorded, if anything.
    pub async fn load(path: Option<PathBuf>) -> anyhow::Result<Self> {
        let tags = match &path {
            Some(path) if path.exists() => serde_json::from_slice(&tokio::fs::read(path).await?)
                .with_context(|| {
                    format!(
                        "reading {}: one written by an older platform can be deleted",
                        path.display()
                    )
                })?,
            _ => BTreeMap::new(),
        };
        Ok(Self {
            tags: Mutex::new(tags),
            path,
        })
    }

    pub async fn replay(&self, tag: &str, asked: &Asked) -> Replay {
        match self.tags.lock().await.get(tag) {
            None => Replay::New,
            Some(quoted) if &quoted.asked == asked => Replay::Same(quoted.answer.clone()),
            Some(_) => Replay::Conflict,
        }
    }

    /// Record a new tag's payout, and write the whole record down before it is answered.
    pub async fn record(&self, tag: &str, quoted: Quoted) -> anyhow::Result<()> {
        let mut tags = self.tags.lock().await;
        tags.insert(tag.to_string(), quoted);
        self.write(&tags).await
    }

    /// Record a quote that replaced one expired unpaid, and write it down before it is used.
    pub async fn requoted(&self, tag: &str, quote: QuoteMade) -> anyhow::Result<()> {
        let mut tags = self.tags.lock().await;
        tags.get_mut(tag)
            .with_context(|| format!("no deal is tagged {tag}"))?
            .quotes
            .push(quote);
        self.write(&tags).await
    }

    /// The cosigner has ended this deal at the platform's word.
    pub async fn ended(&self, tag: &str) -> anyhow::Result<()> {
        let mut tags = self.tags.lock().await;
        tags.get_mut(tag)
            .with_context(|| format!("no deal is tagged {tag}"))?
            .ended = true;
        self.write(&tags).await
    }

    pub async fn get(&self, tag: &str) -> Option<Quoted> {
        self.tags.lock().await.get(tag).cloned()
    }

    /// The deal a payout was quoted for, and its tag.
    pub async fn of_request(&self, request_id: &str) -> Option<(String, Quoted)> {
        self.tags
            .lock()
            .await
            .iter()
            .find(|(_, quoted)| quoted.request_id == request_id)
            .map(|(tag, quoted)| (tag.clone(), quoted.clone()))
    }

    /// When each payout's latest quote expires unpaid, by request id.
    pub async fn expiries(&self) -> BTreeMap<String, u64> {
        self.tags
            .lock()
            .await
            .values()
            .filter_map(|q| Some((q.request_id.clone(), q.quotes.last()?.expires_at)))
            .collect()
    }

    /// Whole file, then rename: a record either lands or it does not.
    async fn write(&self, tags: &BTreeMap<String, Quoted>) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let temp = path.with_extension("tmp");
        tokio::fs::write(&temp, serde_json::to_vec_pretty(tags)?).await?;
        tokio::fs::rename(&temp, path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alices() -> Asked {
        Asked {
            escrow_key: "02aa".into(),
            country: "NG".into(),
            rail: "bank".into(),
            fields: [("accountNumber", "0123456789"), ("bankName", "OPay")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            full_name: "Ada Obi".into(),
            amount_minor: 3_000_000,
        }
    }

    fn quote(id: &str, expires_at: u64) -> QuoteMade {
        QuoteMade {
            id: id.into(),
            transaction_id: format!("Transaction:{id}"),
            cost_micro_usdb: 23_005_382,
            expires_at,
        }
    }

    #[tokio::test]
    async fn a_tag_is_one_payout_asked_for_any_number_of_times() {
        let dir = std::env::temp_dir().join(format!("merlin-deals-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("deals.json");
        let deals = Deals::load(Some(path.clone())).await.unwrap();

        assert_eq!(deals.replay("t1", &alices()).await, Replay::New);
        let answer = serde_json::json!({ "request_id": "reimb-0001" });
        let quoted = Quoted {
            asked: alices(),
            answer: answer.clone(),
            request_id: "reimb-0001".into(),
            account_id: "ExternalAccount:1".into(),
            quotes: vec![quote("Quote:1", 1_000)],
            ended: false,
        };
        deals.record("t1", quoted).await.unwrap();
        deals.requoted("t1", quote("Quote:2", 2_000)).await.unwrap();

        // Asked again, even after a restart, it is the same payout — and its latest quote.
        let reloaded = Deals::load(Some(path)).await.unwrap();
        assert_eq!(reloaded.replay("t1", &alices()).await, Replay::Same(answer));
        let (tag, deal) = reloaded.of_request("reimb-0001").await.unwrap();
        assert_eq!((tag.as_str(), deal.quotes.len()), ("t1", 2));
        assert_eq!(reloaded.expiries().await["reimb-0001"], 2_000);

        // The same tag for another escrow is refused: one payout cannot answer for two deals.
        let bobs = Asked {
            escrow_key: "02bb".into(),
            ..alices()
        };
        assert_eq!(reloaded.replay("t1", &bobs).await, Replay::Conflict);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
