//! Every deal tag this platform has quoted, and what it answered.
//!
//! A tag is the app's name for one payout, and the sealed policy pins it. So a repeated request —
//! a retry over a flaky connection — must get the same payout back, not a second quote; and a tag
//! already quoted must never be quoted for anything else. A second payout under one tag is what
//! would let one naira payout satisfy two customers' deals.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// What makes two requests the same payout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asked {
    pub escrow_key: String,
    pub account_number: String,
    pub bank_name: String,
    pub full_name: String,
    pub amount_kobo: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Quoted {
    asked: Asked,
    /// Exactly what the app was sent, policy and all.
    answer: serde_json::Value,
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
            Some(path) if path.exists() => serde_json::from_slice(&tokio::fs::read(path).await?)?,
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

    /// Record a new tag's answer, and write the whole record down before it is sent.
    pub async fn record(
        &self,
        tag: &str,
        asked: Asked,
        answer: serde_json::Value,
    ) -> anyhow::Result<()> {
        let mut tags = self.tags.lock().await;
        tags.insert(tag.to_string(), Quoted { asked, answer });
        let Some(path) = &self.path else {
            return Ok(());
        };
        // Whole file, then rename: a record either lands or it does not.
        let temp = path.with_extension("tmp");
        tokio::fs::write(&temp, serde_json::to_vec_pretty(&*tags)?).await?;
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
            account_number: "0123456789".into(),
            bank_name: "OPay".into(),
            full_name: "Ada Obi".into(),
            amount_kobo: 3_000_000,
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
        deals.record("t1", alices(), answer.clone()).await.unwrap();

        // Asked again, even after a restart, it is the same payout.
        let reloaded = Deals::load(Some(path)).await.unwrap();
        assert_eq!(reloaded.replay("t1", &alices()).await, Replay::Same(answer));

        // The same tag for another escrow is refused: one payout cannot answer for two deals.
        let bobs = Asked {
            escrow_key: "02bb".into(),
            ..alices()
        };
        assert_eq!(reloaded.replay("t1", &bobs).await, Replay::Conflict);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
