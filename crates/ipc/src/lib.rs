//! JSON-lines IPC: worker → orch (stdout), orch → worker (stdin).
//! Имена `name` / `type` совместимы с 502b worker_threads protocol где возможно.

pub mod pretty_log;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Конфиг, который орк передаёт воркеру при старте (первая строка stdin).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotBoot {
    pub username: String,
    pub password: String,
    pub anarchy: u16,
    pub item: String,
    pub go_type: String,
    pub clan_mates: u32,
    /// socks5://user:pass@host:port
    pub proxy: String,
    pub server: String,
    /// Ник овнера из clan-owners.json (для «чужой клан» на старте).
    #[serde(default)]
    pub expected_clan_owner: Option<String>,
}

/// Worker → orch
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkerOut {
    Named(WorkerMsg),
    /// Сырой текст / алерт (как string postMessage в node)
    Text(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerMsg {
    pub name: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub items: Option<Value>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub lots: Option<Value>,
    #[serde(flatten)]
    pub extra: Value,
}

/// Orch → worker
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OrchCmd {
    #[serde(rename = "update")]
    Update {
        #[serde(default)]
        data: Vec<Value>,
        #[serde(default)]
        all: Vec<Value>,
    },
    #[serde(rename = "items_buying")]
    ItemsBuying { data: Value },
    #[serde(rename = "capital_plan")]
    CapitalPlan {
        #[serde(default, rename = "keepSum")]
        keep_sum: Option<f64>,
        #[serde(default, rename = "maxBuyPrice")]
        max_buy_price: Option<f64>,
    },
    #[serde(rename = "listing_state")]
    ListingState { data: Value },
    #[serde(rename = "listing_res")]
    ListingRes {
        #[serde(rename = "reqId")]
        req_id: u64,
        #[serde(default)]
        result: Value,
    },
    #[serde(rename = "owner_banned")]
    OwnerBanned {
        #[serde(default)]
        owner: Option<String>,
    },
    #[serde(rename = "shutdown")]
    Shutdown,
    #[serde(other)]
    Other,
}

pub fn emit(msg: &WorkerMsg) {
    if let Ok(line) = serde_json::to_string(msg) {
        println!("{line}");
    }
}

pub fn emit_text(text: &str) {
    println!("{}", serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\"")));
}
