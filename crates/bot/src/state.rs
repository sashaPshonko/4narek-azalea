use std::time::{Duration, Instant};

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
    /// Покупка брони «для себя» (wear.rs): в Go не отправляется.
    pub wear: Option<WearBuy>,
}

#[derive(Debug, Clone)]
pub struct WearBuy {
    pub armor_slot: usize,
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

    /// Перерыв «в хабе»: бот онлайн, но стоит в /hub и ничего не делает.
    pub parked: bool,
    pub park_hub_sent: Option<std::time::Instant>,

    pub max_buy_price: Option<f64>,
    pub capital_keep: Option<f64>,
    pub balance: Option<f64>,
    /// Последний баланс из строки «Монет» сайдбара (приходит сам, без `/balance`).
    pub sidebar_balance: Option<f64>,

    pub afk: bool,

    pub sell_ack: SellAck,
    /// Сервер сказал «Не удалось выставить…» (АХ/хранилище забито). Липкий: не теряется, если ответ пришёл позже таймаута ack.
    pub sell_full: bool,
    /// Орк ведёт выравнивание балансов (у кого-то < 13кк): излишки не шлём, казначей не раздаёт.
    pub equalizing: bool,
    /// Приказ орка: перевести деньги другому боту (выравнивание балансов).
    pub pending_peer_pay: Option<(String, i64)>,
    pub need_sell_price: Option<f64>,

    pub listings: ListingMemory,

    /// Go сменил priceSell категории → сброс витрины через хранилище.
    pub need_reset: bool,

    /// «Пора продать / раздать излишек». Ставится:
    ///   - после покупки (`[☃] Вы успешно купили`);
    ///   - после продажи (`[☃] У Вас купили`);
    ///   - когда казначей увидел myNick в табе/чате (см. main.rs).
    /// `ah::run_loop` увидит флаг и вызовет `sell_pass`.
    pub need_sell: bool,
    pub need_reload_ah: bool,

    pub need_rtp: bool,
    pub has_dangerous_trash: bool,

    /// Когда последний раз реально перевыставили витрину. Обновляется ТОЛЬКО по сообщению
    /// «[✔] Предметы успешно перевыставлены!» (main.rs), а не при решении о сбросе: клик мог не пройти.
    pub last_storage_reset: Instant,
    /// Когда последний раз нажали кнопку сброса (ждём сообщение): не жмём повторно, пока оно не пришло.
    pub last_reset_click_at: Option<Instant>,
    /// container id окна хранилища, в котором бот сейчас работает (None = не в хранилище).
    pub storage_win_id: Option<i32>,

    /// Временное окно для текущей обработки хранилища.
    /// Используется main.rs, поэтому поле пока сохраняем.
    pub storage_unlist_until: Option<Instant>,

    /// Как JS storageStaleUnlist* — один и тот же слот/GUI
    /// не снимается → reload 50.
    pub storage_stale_unlist_streak: u32,
    pub storage_stale_unlist_slot: i32,
    pub storage_stale_unlist_key: String,

    /// Прочность предмета в руке при /ah sell
    /// (FunTime max < 90% → skip).
    pub held_durability: Option<f64>,

    /// UUID уже ушедшие в книгу АХ.
    pub ah_book_sent_uuids: std::collections::HashSet<String>,

    pub funauth_verified: bool,

    /// Уже уходим на рестарт из-за funauth —
    /// не спамить exit на каждое сообщение.
    pub funauth_restarting: bool,

    /// Аренда слотов АХ (rent.rs). Факт аренды хранится в файле `rent-state/<ник>.json`, поэтому после
    /// перезапуска бот не арендует повторно: `rent_until_ms` (unix-мс, 0 = не арендовано) и
    /// `rent_last_ms` (когда арендовали в последний раз) подгружаются из файла один раз (`rent_loaded`).
    /// Ещё: когда можно пробовать снова после неудачи и нужно ли сделать дамп окна хранилища после аренды.
    pub rent_until_ms: u64,
    pub rent_last_ms: u64,
    pub rent_loaded: bool,
    /// Ждём в чате «Успешная покупка!» после клика по подтверждению аренды / пришло ли оно.
    pub rent_awaiting_confirm: bool,
    pub rent_confirmed: bool,
    pub rent_retry_at: Instant,
    pub rent_dump_pending: bool,

    /// Режим «инвиз» (invis.rs): включён ли и строки чата, которые он разбирает (время, текст).
    pub invis_on: bool,
    pub invis_chat: Vec<(Instant, String)>,

    pub captcha_busy: bool,
    pub captcha_ok: bool,
    pub captcha_wrong: bool,

    pub last_try_sell_price: Option<f64>,

    /// Scoreboard с номером анки.
    pub on_anarchy: bool,
    pub anarchy_joined_at: Option<Instant>,

    /// Последний пакет скорборда, где видели номер анки.
    pub last_anarchy_board_at: Option<Instant>,

    /// Старт воркера (игнор ⚡ первые 60с).
    pub worker_started_at: Instant,

    pub banned: bool,

    pub buying_item: BuyingItem,

    /// Слоты брони (5–8), под которые надо купить позорную броню (wear.rs).
    pub wear_missing: Vec<usize>,

    pub promenade: PromenadeState,

    /// Каталог id предмета в руке при max/min FunTime.
    pub held_sell_id: Option<String>,
    pub held_sell_price: Option<f64>,
    pub held_nacenka: Option<f64>,
    pub held_book_floor: Option<f64>,
    pub held_book_mid: Option<f64>,

    pub storage_presence_sent: bool,

    /// UUID уже виденных myNick в табе — дедуп, чтобы не триггерить
    /// owner несколько раз при каждом обновлении PlayerInfo.
    pub seen_my_nicks: std::collections::HashSet<uuid::Uuid>,

    /// Когда в последний раз owner платил myNick (CD 60с).
    pub last_my_nick_pay: Option<Instant>,

    /// Дедуп my_nick_seen из чата: не чаще раза в 60с.
    /// Сейчас **не используется** — чат-путь шлёт без дедупа,
    /// орк гасит дубли через `owner_running`. Поле оставлено для будущего.
    pub last_my_nick_seen: Option<Instant>,

    /// Онлайн-ники из таба. Заполняется из PlayerInfoUpdate (ADD_PLAYER),
    /// очищается из PlayerInfoRemove (uuid). Lowercase для сравнения.
    /// Овнер/казначей использует это, чтобы не палиться `/pay` в пустоту.
    pub tab_names: std::collections::HashSet<String>,

    /// Последний полученный перевод от farm-бота (owner слушает в Collect):
    /// (от кого, сумма, когда). Парсится в main.rs из чата
    /// `[$] От игрока <nick> получено +$N`.
    pub last_payment: Option<(String, f64, Instant)>,

    // ============================================================
    // КАЗНАЧЕЙ (роль овнера, передаваемая между farm-ботами)
    // ============================================================

    /// Я сейчас казначей? Обновляется орком через `TreasurerRole`.
    pub is_treasurer: bool,

    /// Ник текущего казначея (может быть не я). Куда слать `/pay`,
    /// когда у меня излишек. Обновляется орком через `TreasurerRole`.
    pub treasurer_nick: Option<String>,

    /// Мне приказано отдать всё (`bal - KEEP_SUM`) этому нику.
    /// Выставляется орком через `FlushToTreasurer` перед сменой роли.
    /// Пока `Some(_)`, фоновый `flush_treasurer` долбит `/pay` до успеха
    /// (без таймаута — орк сам не даст уйти в сон, пока не придёт
    /// `TreasurerFlushed { ok: true }`).
    pub treasurer_flush_to: Option<String>,

    /// Флаг «`flush_treasurer` уже запущен» — чтобы не спавнить второй поток.
    pub treasurer_flush_in_flight: bool,

    /// Последний известный снимок балансов от орка — для redistribute,
    /// когда я казначей. `(nick, balance)`; `-1.0` = «неизвестно».
    pub balances_snapshot: Vec<(String, f64)>,

    /// Сколько последний раз получилось отправить при flush (для отчёта).
    pub treasurer_last_flush_amount: Option<i64>,

    /// Когда последний раз успешно завершили flush (для логов).
    pub treasurer_last_flush_at: Option<Instant>,
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

/// Период сброса витрины (раз в 60 с, как JS `resetime > 60`).
pub const RESET_PERIOD: Duration = Duration::from_secs(60);
/// Принудительную продажу не начинаем, если до сброса меньше этого: проход продажи длится десятки секунд и сдвинул бы ритм.
pub const SALE_RESET_GUARD: Duration = Duration::from_secs(15);
/// Не жмём сброс чаще, чем раз в столько, пока не пришло подтверждение (и не ходим в хранилище ради повтора чаще).
pub const RESET_RETRY: Duration = Duration::from_secs(10);

/// Сколько осталось до следующего сброса витрины.
pub fn reset_remaining(since_last_reset: Duration) -> Duration {
    RESET_PERIOD.saturating_sub(since_last_reset)
}

/// Пора ли сбрасывать витрину: прошло > 60 с с последнего подтверждённого сброса и недавно кнопку не нажимали.
pub fn reset_due(since_last_reset: Duration, since_click: Option<Duration>) -> bool {
    since_last_reset > RESET_PERIOD && since_click.map_or(true, |c| c >= RESET_RETRY)
}

/// За сколько до срока заходим в хранилище. Путь до кнопки — пауза перед кликом на хранилище (1–2 с,
/// STORAGE_ENTER) + открытие окна (доли секунды) + 150 мс: до ~3 с. Входим за 4 с и ждём точного момента.
pub const RESET_PRE_OPEN: Duration = Duration::from_secs(4);
/// Запас после 60 с до клика по кнопке сброса: наш отсчёт идёт от сообщения-подтверждения, оно приходит чуть
/// позже сервера, но кликать на долю секунды раньше КД нельзя (потеряем RESET_RETRY = 10 с).
pub const RESET_CLICK_MARGIN: Duration = Duration::from_millis(250);

/// Пора ли заходить в хранилище ради сброса: до срока осталось не больше `RESET_PRE_OPEN` (или он уже настал).
pub fn reset_due_soon(since_last_reset: Duration, since_click: Option<Duration>) -> bool {
    since_last_reset + RESET_PRE_OPEN >= RESET_PERIOD && since_click.map_or(true, |c| c >= RESET_RETRY)
}

/// Случайная добавка к моменту клика (мс): чтобы сброс шёл не ровно по 60 с, а с разбросом ~0,3–2,5 с.
pub const RESET_JITTER_MS: std::ops::RangeInclusive<u64> = 300..=2500;

/// Сколько ещё ждать до момента клика по кнопке сброса (0 — можно кликать). `jitter` — случайная добавка.
pub fn reset_click_wait(since_last_reset: Duration, jitter: Duration) -> Duration {
    (RESET_PERIOD + RESET_CLICK_MARGIN + jitter).saturating_sub(since_last_reset)
}

/// Блокирует ли близкий сброс принудительную продажу: до сброса < 15 с (или он просрочен).
/// Пока кнопку сброса только что нажали (меньше RESET_RETRY) и подтверждения нет — продажа тоже ждёт: уход из окна
/// до сообщения «Предметы успешно перевыставлены» срывал сброс, и таймер съезжал.
pub fn sale_blocked_by_reset(since_last_reset: Duration, since_click: Option<Duration>) -> bool {
    sale_blocked_by_reset_lots(since_last_reset, since_click, usize::MAX)
}

/// Меньше стольких лотов на витрине — продажу не блокируем за последние 15 с перед сбросом: пустые слоты стоят
/// дороже, чем сдвиг сброса на 10–20 с для одного-двух лотов. Пока ждём подтверждение нажатого сброса или сброс
/// уже просрочен, блок остаётся при любом числе лотов.
pub const FEW_LOTS_ALLOW_SALE: usize = 3;

pub fn sale_blocked_by_reset_lots(
    since_last_reset: Duration,
    since_click: Option<Duration>,
    lots_on_shelf: usize,
) -> bool {
    if since_click.map_or(false, |c| c < RESET_RETRY) {
        return true;
    }
    if lots_on_shelf < FEW_LOTS_ALLOW_SALE && since_last_reset < RESET_PERIOD {
        return false;
    }
    reset_remaining(since_last_reset) < SALE_RESET_GUARD
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
            parked: false,
            park_hub_sent: None,

            max_buy_price: None,
            capital_keep: None,
            balance: None,
            sidebar_balance: None,

            afk: false,

            sell_ack: SellAck::Idle,
            sell_full: false,
            equalizing: false,
            pending_peer_pay: None,
            need_sell_price: None,

            listings: ListingMemory::default(),

            need_reset: false,
            need_sell: false,
            need_reload_ah: false,

            need_rtp: false,
            has_dangerous_trash: false,

            last_storage_reset: Instant::now(),
            last_reset_click_at: None,
            storage_win_id: None,

            storage_unlist_until: None,

            storage_stale_unlist_streak: 0,
            storage_stale_unlist_slot: -1,
            storage_stale_unlist_key: String::new(),

            held_durability: None,

            ah_book_sent_uuids:
                std::collections::HashSet::new(),

            funauth_verified: false,
            funauth_restarting: false,

            rent_until_ms: 0,
            rent_last_ms: 0,
            rent_loaded: false,
            rent_awaiting_confirm: false,
            rent_confirmed: false,
            rent_retry_at: Instant::now(),
            rent_dump_pending: false,
            invis_on: false,
            invis_chat: Vec::new(),

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

            wear_missing: Vec::new(),

            promenade: PromenadeState::default(),

            held_sell_id: None,
            held_sell_price: None,
            held_nacenka: None,
            held_book_floor: None,
            held_book_mid: None,

            storage_presence_sent: false,

            seen_my_nicks: std::collections::HashSet::new(),
            last_my_nick_pay: None,
            last_my_nick_seen: None,

            tab_names: std::collections::HashSet::new(),

            last_payment: None,

            // казначей: стартовые значения из boot
            is_treasurer: boot.treasurer_nick
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case(&boot.username))
                .unwrap_or(false),
            treasurer_nick: boot.treasurer_nick.clone(),
            treasurer_flush_to: None,
            treasurer_flush_in_flight: false,
            balances_snapshot: Vec::new(),
            treasurer_last_flush_amount: None,
            treasurer_last_flush_at: None,
        }
    }

    pub fn apply_cmd(&mut self, cmd: OrchCmd) {
        match cmd {
            OrchCmd::Update { data, all } => {
                if catalog_prices_changed(&self.catalog, &data) {
                    self.need_reset = true;

                    tracing::info!(
                        "цены Go → needReset (изменилась priceSell)"
                    );
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
                self.capital_keep =
                    keep_sum.filter(|v| *v > 0.0);

                self.max_buy_price =
                    max_buy_price.filter(|v| *v > 0.0);
            }

            OrchCmd::ListingState { data } => {
                self.listings.import_state(&data);

                tracing::debug!(
                    "listing state imported"
                );
            }

            OrchCmd::ListingRes { req_id, result } => {
                let _ = (req_id, result);
            }

            OrchCmd::Pay { .. } => {
                // Обрабатывается в boot::stdin_loop → pay_tx → owner::run_loop.
                // В BotState ничего не делаем.
            }

            OrchCmd::Equalizing { on } => {
                self.equalizing = on;
            }

            OrchCmd::PayPeer { to, amount } => {
                self.pending_peer_pay = Some((to, amount));
            }

            OrchCmd::BalancesSnapshot { balances } => {
                // Казначей получает снимок балансов и раздаёт в sell_pass.
                // Сохраняем — `ah::sell_pass` читает.
                self.balances_snapshot = balances;
            }

            OrchCmd::TreasurerRole { username } => {
                // Орк переназначил казначея.
                let new_nick = if username.is_empty() {
                    None
                } else {
                    Some(username)
                };
                self.is_treasurer = new_nick
                    .as_deref()
                    .map(|t| t.eq_ignore_ascii_case(&self.username))
                    .unwrap_or(false);
                self.treasurer_nick = new_nick;

                // Глобальный флаг для логов: все `info!`/`warn!`/`line`
                // этого процесса будут magenta, если я казначей.
                ipc::pretty_log::set_treasurer(self.is_treasurer);
            }

            OrchCmd::FlushToTreasurer { to } => {
                // Орк приказал отдать всё этому нику. Фоновый
                // `flush_treasurer` в main.rs запустится и будет
                // долбить `/pay` до успеха (без таймаута).
                if to.is_empty() {
                    self.treasurer_flush_to = None;
                } else {
                    self.treasurer_flush_to = Some(to);
                }
                // Флаг in-flight не ставим здесь — его выставит main.rs
                // при старте потока, чтобы не плодить второй.
            }

            OrchCmd::MyNickSeen { .. } => {
                // Не используется (нет отдельного овнера).
            }

            OrchCmd::TreasurerBalanceRequest => {
                // Орк просит баланс казначея. Ответ шлёт `flush_treasurer_loop`
                // / `sell_pass` по факту `/balance`. Здесь ничего не делаем.
            }

            OrchCmd::Shutdown => {
                self.shutdown = true;
            }

            OrchCmd::Park { on } => {
                self.parked = on;
                if !on {
                    self.park_hub_sent = None;
                }
            }
            OrchCmd::Other => {}
        }
    }
}

fn catalog_prices_changed(
    prev: &[Value],
    next: &[Value],
) -> bool {
    if next.is_empty() {
        return false;
    }

    let mut prev_by_id =
        std::collections::HashMap::new();

    for item in prev {
        let Some(id) =
            item.get("id").and_then(|v| v.as_str())
        else {
            continue;
        };

        let Some(p) =
            item.get("priceSell").and_then(|v| v.as_f64())
        else {
            continue;
        };

        prev_by_id.insert(id.to_string(), p);
    }

    if prev_by_id.is_empty() {
        return false;
    }

    for item in next {
        let Some(id) =
            item.get("id").and_then(|v| v.as_str())
        else {
            continue;
        };

        let Some(p) =
            item.get("priceSell").and_then(|v| v.as_f64())
        else {
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

pub fn uuid_blocked_by_other(
    items_buying: &Value,
    uuid: &str,
    my_username: &str,
) -> bool {
    let Some(arr) = items_buying.as_array() else {
        return false;
    };

    for raw in arr {
        let (u, owner) = match raw {
            Value::String(s) => (s.as_str(), None),

            Value::Object(o) => (
                o.get("uuid")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),

                o.get("username")
                    .and_then(|v| v.as_str()),
            ),

            _ => continue,
        };

        if u != uuid {
            continue;
        }

        if owner.is_none()
            || owner != Some(my_username)
        {
            return true;
        }
    }

    false
}

impl BotState {
    pub fn reset_due_now(&self) -> bool {
        reset_due(
            self.last_storage_reset.elapsed(),
            self.last_reset_click_at.map(|t| t.elapsed()),
        )
    }

    pub fn reset_soon_now(&self) -> bool {
        reset_due_soon(
            self.last_storage_reset.elapsed(),
            self.last_reset_click_at.map(|t| t.elapsed()),
        )
    }

    pub fn sale_blocked_by_reset_now(&self) -> bool {
        sale_blocked_by_reset_lots(
            self.last_storage_reset.elapsed(),
            self.last_reset_click_at.map(|t| t.elapsed()),
            self.listings.all_metas().len(),
        )
    }
}

#[cfg(test)]
mod reset_timing_tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn reset_is_due_after_a_minute_without_recent_click() {
        assert!(!reset_due(s(59), None));
        assert!(reset_due(s(61), None));
        assert!(!reset_due(s(90), Some(s(3)))); // только что нажали — ждём сообщение
        assert!(reset_due(s(90), Some(s(12)))); // сообщение не пришло — повторяем
    }

    #[test]
    fn storage_opens_4s_early_but_click_waits_for_60s() {
        assert!(!reset_due_soon(s(55), None)); // осталось 5 с — рано
        assert!(reset_due_soon(s(56), None)); // осталось 4 с — заходим
        assert!(!reset_due_soon(s(55), Some(s(3)))); // только что нажали — ждём сообщение
        assert_eq!(reset_click_wait(s(55), Duration::ZERO), Duration::from_millis(5250));
        assert_eq!(reset_click_wait(s(55), Duration::from_millis(1000)), Duration::from_millis(6250)); // с добавкой
        assert_eq!(reset_click_wait(s(63), Duration::from_millis(1000)), Duration::ZERO); // срок прошёл — кликаем сразу
    }

    #[test]
    fn sale_waits_when_reset_is_less_than_15s_away() {
        assert!(!sale_blocked_by_reset(s(40), None)); // осталось 20 с — можно продавать
        assert!(!sale_blocked_by_reset(s(45), None)); // ровно 15 с — ещё можно
        assert!(sale_blocked_by_reset(s(46), None)); // осталось 14 с — ждём сброс
        assert!(sale_blocked_by_reset(s(70), None)); // сброс просрочен — сначала он
    }

    #[test]
    fn sale_waits_for_reset_confirmation_after_pressing_it() {
        assert!(sale_blocked_by_reset(s(70), Some(s(2)))); // нажали, ждём сообщение — не уходим продавать
        assert!(sale_blocked_by_reset(s(10), Some(s(2)))); // даже если до следующего сброса далеко
        assert!(sale_blocked_by_reset(s(70), Some(s(15)))); // сообщения всё нет — сброс всё ещё впереди
        assert!(!sale_blocked_by_reset(s(10), None)); // подтверждение пришло, сброс далеко — можно продавать
    }

    #[test]
    fn few_lots_on_shelf_do_not_block_sale_before_reset() {
        // 2 лота, до сброса 10 с — продаём
        assert!(!sale_blocked_by_reset_lots(s(50), None, 2));
        // 3 лота и больше — блок как раньше
        assert!(sale_blocked_by_reset_lots(s(50), None, 3));
        assert!(sale_blocked_by_reset_lots(s(50), None, 5));
        // сброс просрочен — сначала он, даже при пустой витрине
        assert!(sale_blocked_by_reset_lots(s(61), None, 0));
        // только что нажали сброс и ждём подтверждение — не уходим, даже при 0 лотов
        assert!(sale_blocked_by_reset_lots(s(40), Some(s(2)), 0));
    }
}
