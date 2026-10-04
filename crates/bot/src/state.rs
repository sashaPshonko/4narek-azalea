use std::time::Instant;

use ipc::{BotBoot, OrchCmd};
use serde_json::Value;

use crate::listing::ListingMemory;
use crate::warp::PromenadeState;

#[derive(Debug, Clone, Default)]
pub struct BuyingItem {
    pub id: String,
    pub price: f64,
    pub buy_price: f64,
    pub nacenka: f64,
    pub enchants: Value,
    pub durability: Option<f64>,
}

pub struct BotState {
    /// Ник из boot — для IPC без прокидывания Boot везде.
    pub username: String,
    pub catalog: Vec<Value>,
    pub catalog_all: Vec<Value>,
    pub items_buying: Value,
    pub last_window_title: String,
    pub last_window_id: i32,
    pub window_tick: Option<Instant>,
    pub shutdown: bool,
    pub max_buy_price: Option<f64>,
    pub capital_keep: Option<f64>,
    pub balance: Option<f64>,
    pub afk: bool,
    pub sell_ack: SellAck,
    pub need_sell_price: Option<f64>,
    pub listings: ListingMemory,
    pub need_sell: bool,
    /// Go сменил priceSell категории → сброс витрины через хранилище (как JS needReset).
    pub need_reset: bool,
    /// Последний цикл КД-снятия всех лотов (как JS lastUnlistCycleAt).
    pub last_unlist_cycle_at: Option<Instant>,
    /// Пауза до следующего КД-снятия, мс (2–4 мин).
    pub unlist_cycle_gap_ms: u64,
    pub need_reload_ah: bool,
    pub need_rtp: bool,
    pub has_dangerous_trash: bool,
    pub last_storage_reset: Instant,
    pub storage_unlist_until: Option<Instant>,
    /// Как JS storageStaleUnlist* — один и тот же слот/GUI не снимается → reload 50.
    pub storage_stale_unlist_streak: u32,
    pub storage_stale_unlist_slot: i32,
    pub storage_stale_unlist_key: String,
    /// Прочность предмета в руке при /ah sell (FunTime max < 90% → skip).
    pub held_durability: Option<f64>,
    /// UUID уже ушедшие в книгу АХ (как JS ahBookSentUuids).
    pub ah_book_sent_uuids: std::collections::HashSet<String>,
    pub last_clan_invest: Option<Instant>,
    pub awaiting_withdraw: bool,
    pub treasury_empty_reported: bool,
    pub funauth_verified: bool,
    /// Уже уходим на рестарт из‑за funauth — не спамить exit на каждое сообщение.
    pub funauth_restarting: bool,
    pub captcha_busy: bool,
    pub captcha_ok: bool,
    pub captcha_wrong: bool,
    pub last_try_sell_price: Option<f64>,
    /// Scoreboard с номером анки (как timeJoinAnarchy в old).
    pub on_anarchy: bool,
    pub anarchy_joined_at: Option<Instant>,
    /// Последний пакет скорборда, где видели номер анки.
    pub last_anarchy_board_at: Option<Instant>,
    /// Старт воркера (игнор ⚡ первые 60с).
    pub worker_started_at: Instant,
    pub banned: bool,
    pub buying_item: BuyingItem,
    pub owner_ban_drain: bool,
    pub owner_ban_leave_done: bool,
    pub promenade: PromenadeState,
    /// Каталог id предмета в руке при max/min FunTime.
    pub held_sell_id: Option<String>,
    pub held_sell_price: Option<f64>,
    pub held_nacenka: Option<f64>,
    pub held_book_floor: Option<f64>,
    pub held_book_mid: Option<f64>,
    /// /clan info
    pub clan_leader: Option<String>,
    pub not_in_clan: bool,
    pub pending_clan_leave_confirm: Option<String>,
    pub clan_accept_busy: bool,
    pub foreign_clan_busy: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SellAck {
    #[default]
    Idle,
    Waiting,
    Ok,
    Full,
    Empty,
    Skip,
    Retry,
    /// FunTime: «слишком дорого… введите команду ещё раз»
    Confirm,
}

impl BotState {
    pub fn from_boot(boot: &BotBoot) -> Self {
        Self {
            username: boot.username.clone(),
            catalog: Vec::new(),
            catalog_all: Vec::new(),
            items_buying: Value::Array(vec![]),
            last_window_title: String::new(),
            last_window_id: 0,
            window_tick: None,
            shutdown: false,
            max_buy_price: None,
            capital_keep: None,
            balance: None,
            afk: false,
            sell_ack: SellAck::Idle,
            need_sell_price: None,
            listings: ListingMemory::default(),
            need_sell: false,
            need_reset: false,
            last_unlist_cycle_at: Some(Instant::now()),
            unlist_cycle_gap_ms: 150_000,
            need_reload_ah: false,
            need_rtp: false,
            has_dangerous_trash: false,
            last_storage_reset: Instant::now(),
            storage_unlist_until: None,
            storage_stale_unlist_streak: 0,
            storage_stale_unlist_slot: -1,
            storage_stale_unlist_key: String::new(),
            held_durability: None,
            ah_book_sent_uuids: std::collections::HashSet::new(),
            last_clan_invest: None,
            awaiting_withdraw: false,
            treasury_empty_reported: false,
            funauth_verified: false,
            funauth_restarting: false,
            captcha_busy: false,
            captcha_ok: false,
            captcha_wrong: false,
            last_try_sell_price: None,
            on_anarchy: false,
            anarchy_joined_at: None,
            last_anarchy_board_at: None,
            worker_started_at: Instant::now(),
            banned: false,
            buying_item: BuyingItem::default(),
            owner_ban_drain: false,
            owner_ban_leave_done: false,
            promenade: PromenadeState::default(),
            held_sell_id: None,
            held_sell_price: None,
            held_nacenka: None,
            held_book_floor: None,
            held_book_mid: None,
            clan_leader: None,
            not_in_clan: false,
            pending_clan_leave_confirm: None,
            clan_accept_busy: false,
            foreign_clan_busy: false,
        }
    }

    pub fn apply_cmd(&mut self, cmd: OrchCmd) {
        match cmd {
            OrchCmd::Update { data, all } => {
                if catalog_prices_changed(&self.catalog, &data) {
                    self.need_reset = true;
                    tracing::info!("цены Go → needReset (изменилась priceSell)");
                }
                tracing::debug!(
                    "каталог update: {} позиций (all={})",
                    data.len(),
                    all.len()
                );
                self.catalog = data;
                self.catalog_all = if all.is_empty() {
                    self.catalog.clone()
                } else {
                    all
                };
            }
            OrchCmd::ItemsBuying { data } => {
                self.items_buying = data;
            }
            OrchCmd::CapitalPlan {
                keep_sum,
                max_buy_price,
            } => {
                self.capital_keep = keep_sum.filter(|v| *v > 0.0);
                self.max_buy_price = max_buy_price.filter(|v| *v > 0.0);
            }
            OrchCmd::ListingState { data } => {
                self.listings.import_state(&data);
                tracing::debug!("listing state imported");
            }
            OrchCmd::ListingRes { req_id, result } => {
                let _ = (req_id, result);
            }
            OrchCmd::OwnerBanned { owner } => {
                if !self.owner_ban_drain {
                    self.owner_ban_drain = true;
                    tracing::warn!(
                        "овнер забанен{} → казна и leave",
                        owner
                            .as_ref()
                            .map(|o| format!(" ({o})"))
                            .unwrap_or_default()
                    );
                }
            }
            OrchCmd::Shutdown => self.shutdown = true,
            OrchCmd::Other => {}
        }
    }

    /// Как JS `unlistCycleDue` — пора снять витрину для ротации.
    pub fn unlist_cycle_due(&self) -> bool {
        let Some(last) = self.last_unlist_cycle_at else {
            return false;
        };
        last.elapsed().as_millis() as u64 >= self.unlist_cycle_gap_ms.max(60_000)
    }

    pub fn mark_unlist_cycle_done(&mut self) {
        self.last_unlist_cycle_at = Some(Instant::now());
        // 2–4 мин как JS nextUnlistCycleGapMs
        self.unlist_cycle_gap_ms = 120_000 + (rand::random::<u64>() % 120_000);
    }
}

fn catalog_prices_changed(prev: &[Value], next: &[Value]) -> bool {
    if next.is_empty() {
        return false;
    }
    let mut prev_by_id = std::collections::HashMap::new();
    for item in prev {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(p) = item.get("priceSell").and_then(|v| v.as_f64()) else {
            continue;
        };
        prev_by_id.insert(id.to_string(), p);
    }
    if prev_by_id.is_empty() {
        return false;
    }
    for item in next {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(p) = item.get("priceSell").and_then(|v| v.as_f64()) else {
            continue;
        };
        if let Some(&old) = prev_by_id.get(id) {
            if (old - p).abs() > f64::EPSILON {
                return true;
            }
        }
    }
    false
}

pub fn uuid_blocked_by_other(items_buying: &Value, uuid: &str, my_username: &str) -> bool {
    let Some(arr) = items_buying.as_array() else {
        return false;
    };
    for raw in arr {
        let (u, owner) = match raw {
            Value::String(s) => (s.as_str(), None),
            Value::Object(o) => (
                o.get("uuid").and_then(|v| v.as_str()).unwrap_or(""),
                o.get("username").and_then(|v| v.as_str()),
            ),
            _ => continue,
        };
        if u != uuid {
            continue;
        }
        if owner.is_none() || owner != Some(my_username) {
            return true;
        }
    }
    false
}
