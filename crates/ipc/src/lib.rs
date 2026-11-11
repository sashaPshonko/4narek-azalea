//! JSON-lines IPC: worker → orch (stdout), orch → worker (stdin).
//! Имена `name` / `type` совместимы с 502b worker_threads protocol где возможно.

pub mod balance;
pub mod pretty_log;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Зачем орк спавнит овнера. Строкой в `BotBoot.owner_reason`.
/// Сейчас **не используется** — отдельного овнера нет, есть казначей-роль
/// на farm-боте. Оставлено для обратной совместимости.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnerReason {
    /// Любой бот увидел myNick в табе или в чате → owner заходит /balance → /pay myNick.
    #[serde(rename = "my_nick_seen")]
    MyNickSeen,
    /// Есть активный farm-бот с балансом < 13kk → redistribute.
    #[serde(rename = "redistribute")]
    Redistribute,
    /// Есть активный farm-бот с балансом > 30kk → подождать перевод + redistribute.
    #[serde(rename = "collect")]
    Collect,
    /// На всякий (не используется).
    #[serde(rename = "nothing")]
    Nothing,
}

/// Конфиг, который орк передаёт воркеру при старте (первая строка stdin).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotBoot {
    pub username: String,
    pub password: String,
    pub anarchy: u16,
    pub item: String,
    pub go_type: String,
    /// socks5://user:pass@host:port
    pub proxy: String,
    pub server: String,
    /// Глобальный myNick из корня clan-owners.json.
    /// Любой бот при его появлении в табе **или упоминании в чате**
    /// шлёт `my_nick_seen`.
    #[serde(default)]
    pub my_nick: Option<String>,
    /// Ник owner'а этой анки (из clan-owners.json).
    /// Farm-бот по нему делает /pay, когда сливает излишек.
    /// **Устарело** — теперь `treasurer_nick` (роль казначея).
    #[serde(default)]
    pub owner_nick: Option<String>,
    /// Зачем орк спавнит этого овнера. Только для `go_type == "owner"`.
    /// **Устарело** — отдельного овнера нет.
    #[serde(default)]
    pub owner_reason: Option<OwnerReason>,

    // ============================================================
    // КАЗНАЧЕЙ (роль овнера, передаваемая между farm-ботами)
    // ============================================================

    /// Ник текущего казначея на старте. Куда слать `/pay`, когда у меня
    /// излишек. Может быть `None`, если орк ещё не определил казначея
    /// (тогда обновится через `OrchCmd::TreasurerRole`).
    #[serde(default)]
    pub treasurer_nick: Option<String>,
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
    /// Оркестратор → owner-бот: заплати `to` сумму `amount` (дважды с задержкой).
    /// Owner отвечает `WorkerMsg { name: "paid", data: { ok, reqId, amount } }`.
    #[serde(rename = "pay")]
    Pay {
        to: String,
        amount: i64,
        #[serde(rename = "reqId")]
        req_id: u64,
    },
    /// Оркестратор → farm-боту: перекинь `amount` другому активному боту
    /// (выравнивание денег между ботами одной анархии).
    #[serde(rename = "pay_peer")]
    PayPeer { to: String, amount: i64 },
    /// Оркестратор → всем farm-ботам: идёт выравнивание (у кого-то из активных
    /// ботов меньше 13кк). Пока `true` — излишки казначею не шлём, казначей
    /// сам не раздаёт «выше KEEP_SUM»: деньги двигает только выравнивание орка.
    #[serde(rename = "equalizing")]
    Equalizing { on: bool },
    /// Оркестратор → казначей-бот: снимок балансов всех farm-ботов.
    /// Казначей анализирует и решает, кому докинуть до KEEP_SUM.
    /// Шлётся **только** казначею.
    #[serde(rename = "balances_snapshot")]
    BalancesSnapshot {
        /// username → balance
        balances: Vec<(String, f64)>,
    },
    /// Оркестратор → owner-бот: «myNick в табе/чате, зайди и слей излишек».
    /// В `BotBoot.owner_reason = MyNickSeen` уже это знает, но отдельный
    /// cmd позволяет передать ник явно (если орк его получил из чата).
    /// **Устарело** — отдельного овнера нет.
    #[serde(rename = "my_nick_seen")]
    MyNickSeen {
        nick: String,
        #[serde(default)]
        anarchy: u16,
    },

    // ============================================================
    // КАЗНАЧЕЙ
    // ============================================================

    /// Оркестратор → всем farm-ботам: сменился казначей.
    /// `username = ""` — казначея нет (не должны уходить в сон).
    /// Бот-казначей выставляет `is_treasurer = true`,
    /// остальные — `treasurer_nick = Some(username)`.
    #[serde(rename = "treasurer_role")]
    TreasurerRole { username: String },

    /// Оркестратор → бывшему казначею: перед уходом в сон отдай всё
    /// `bal - KEEP_SUM` этому нику. Фоновый `flush_treasurer` долбит
    /// `/pay` **до успеха** (без таймаута) — орк сам не даст уйти в сон,
    /// пока не придёт `WorkerMsg { name: "treasurer_flushed", ok: true }`.
    #[serde(rename = "flush_to_treasurer")]
    FlushToTreasurer { to: String },

    /// Оркестратор → казначею: запрос текущего баланса и суммы, которую
    /// он держит. Ответ — `WorkerMsg { name: "treasurer_balance", ... }`.
    /// Нужно, чтобы орк знал, когда казначей реально всё отдал.
    #[serde(rename = "treasurer_balance_request")]
    TreasurerBalanceRequest,

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
    println!(
        "{}",
        serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""))
    );
}