//! Аренда слотов АХ: `/ah rent` → окно «аренда» → слот 28 → «Выберите валюту» → слот 21 →
//! «подтверждение покупки» → слот 0. Аренда на час, 5 слотов за 1.5 млн.
//!
//! Окна узнаются **по подстроке заголовка без учёта регистра** (и с поправкой на латинские буквы-двойники
//! вроде `a`/`о`, которые FunTime подмешивает в русские слова: «Ценa», «Истeкaeт»).
//!
//! Включено всегда. Настройки — константы ниже (`ENABLED`, `FROM_MSK`, `TO_MSK`, `MINUTES`, `RENEW_BEFORE_SEC`,
//! `MIN_INTERVAL_MIN`, `PRICE`); переменных окружения нет. Окно `FROM_MSK..TO_MSK` можно задать через полночь
//! (например, 22→4).
//!
//! Факт аренды пишется в `rent-state/<ник>.json` (в рабочей папке бота): после перезапуска бот видит, что
//! аренда ещё действует, и не платит повторно. Запись делается **перед** кликом по подтверждению, чтобы
//! сбой между кликом и записью не привёл к двойной оплате; если клик не удался, запись откатывается.
//! Запускается в конце `sell_pass` (`sell.rs`) — там же, где пишется баланс и делаются переводы:
//! каждым ботом отдельно, раз в час, в выгодные часы, если на счёте хватает денег.
//! Успех подтверждается сообщением в чате «Успешная покупка!».
//!
//! Важно: сама аренда слотов выгоды не даёт, пока бот не выставляет больше 5 лотов. Лимиты в `ah.rs`,
//! `listing.rs`, `storage.rs`, `sell.rs` пока остались на 5 (раскладку окна хранилища с 10 слотами ещё надо
//! увидеть в дампе `dumps/storage-window.jsonl`, он пишется автоматически после аренды).

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use azalea::prelude::*;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{delay, safe_inv, state::BotState, titles};

/// Слотов АХ у бота без аренды / с арендой (5 арендованных сверху).
pub const BASE_SLOTS: usize = 5;
pub const RENTED_SLOTS: usize = 10;

pub const SLOT_RENT: usize = 28;
pub const SLOT_CURRENCY: usize = 21;
pub const SLOT_CONFIRM: usize = 0;

/// Сколько ждать появления каждого следующего окна.
const STEP_TIMEOUT: Duration = Duration::from_secs(6);
/// Сколько ждать сообщение «Успешная покупка!» после клика по подтверждению.
const CONFIRM_WAIT: Duration = Duration::from_secs(8);
/// Пауза после неудачной попытки, прежде чем пробовать снова.
const RETRY_BACKOFF: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Окно с заголовком «аренда».
    Rent,
    /// «Выберите валюту».
    Currency,
    /// «Подтверждение покупки».
    Confirm,
}

impl Step {
    fn slot(self) -> usize {
        match self {
            Step::Rent => SLOT_RENT,
            Step::Currency => SLOT_CURRENCY,
            Step::Confirm => SLOT_CONFIRM,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Step::Rent => "аренда",
            Step::Currency => "выбор валюты",
            Step::Confirm => "подтверждение",
        }
    }
}

/// Какой шаг аренды показывает окно с таким заголовком.
///
/// Порядок проверки важен: окно подтверждения проверяем первым (оно же могло бы упоминать «аренду»).
/// Подстроки заголовков живут в одном месте — `titles.rs` (их же использует цикл АХ).
pub fn step_for_title(title: &str) -> Option<Step> {
    if titles::is_confirm(title) {
        Some(Step::Confirm)
    } else if titles::is_currency(title) {
        Some(Step::Currency)
    } else if titles::is_rent(title) {
        Some(Step::Rent)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct RentConfig {
    pub enabled: bool,
    pub from_msk: u32,
    pub to_msk: u32,
    pub minutes: u64,
    pub renew_before: Duration,
    /// Не чаще одной аренды за столько времени.
    pub min_interval: Duration,
    /// Цена аренды, ₽: не арендуем, если известный баланс меньше.
    pub price: f64,
}

/// Настройки аренды. Меняются здесь и пересборкой.
const ENABLED: bool = true;
/// В какие часы МСК арендовать (выгодное окно по доходу слота: слот приносит больше 300 тыс./ч с ~10:00 до 24:00).
const FROM_MSK: u32 = 10;
const TO_MSK: u32 = 24;
/// На сколько минут аренда.
const MINUTES: u64 = 60;
/// За сколько секунд до конца аренды продлевать (0 — когда кончилась; повторная покупка до конца может
/// вести себя по-разному, пока не проверено).
const RENEW_BEFORE_SEC: u64 = 0;
/// Не чаще одной аренды за столько минут.
const MIN_INTERVAL_MIN: u64 = 60;
/// Цена аренды 5 слотов на час, ₽: не арендуем, если известный баланс меньше.
const PRICE: f64 = 1_500_000.0;

impl RentConfig {
    pub fn builtin() -> Self {
        Self {
            enabled: ENABLED,
            from_msk: FROM_MSK.min(24),
            to_msk: TO_MSK.min(24),
            minutes: MINUTES.max(1),
            renew_before: Duration::from_secs(RENEW_BEFORE_SEC),
            min_interval: Duration::from_secs(MIN_INTERVAL_MIN * 60),
            price: PRICE,
        }
    }

    /// Час МСК (0–23) попадает в окно аренды? Окно `from..to`; если `from > to`, то через полночь.
    pub fn hour_allowed(&self, hour_msk: u32) -> bool {
        let (f, t) = (self.from_msk, self.to_msk);
        if f == t {
            return true; // 10→10 понимаем как «весь день»
        }
        if f < t {
            hour_msk >= f && hour_msk < t
        } else {
            hour_msk >= f || hour_msk < t
        }
    }
}

fn config() -> &'static RentConfig {
    static CFG: std::sync::OnceLock<RentConfig> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        let c = RentConfig::builtin();
        if c.enabled {
            info!(
                "аренда слотов ВКЛ: {:02}:00–{:02}:00 МСК, на {} мин, не чаще раза в {} мин, продление за {} с",
                c.from_msk,
                c.to_msk,
                c.minutes,
                c.min_interval.as_secs() / 60,
                c.renew_before.as_secs()
            );
        }
        c
    })
}

/// Текущий час по МСК (UTC+3).
fn hour_msk_now() -> u32 {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    (((ms + 3 * 3600 * 1000) / 3_600_000) % 24) as u32
}

/// Запись о последней аренде (файл `rent-state/<ник>.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RentRecord {
    /// Когда арендовали, unix-мс.
    pub rented_at_ms: u64,
    /// До какого момента действует, unix-мс.
    pub until_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Ник → безопасное имя файла.
fn safe_name(username: &str) -> String {
    username
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

fn store_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("rent-state")
}

fn record_path(dir: &std::path::Path, username: &str) -> PathBuf {
    dir.join(format!("{}.json", safe_name(username)))
}

fn load_record_in(dir: &std::path::Path, username: &str) -> Option<RentRecord> {
    let raw = std::fs::read_to_string(record_path(dir, username)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Запись атомарно (во временный файл и переименование): сбой посередине не оставит битый JSON.
fn save_record_in(dir: &std::path::Path, username: &str, rec: &RentRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = record_path(dir, username);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(rec).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)
}

fn delete_record_in(dir: &std::path::Path, username: &str) {
    let _ = std::fs::remove_file(record_path(dir, username));
}

/// Аренда ещё действует (по файлу/состоянию)? Пригодится, когда лимит слотов станет зависеть от аренды.
#[allow(dead_code)]
pub fn active(state: &Arc<Mutex<BotState>>) -> bool {
    state.lock().rent_until_ms > now_ms()
}

/// Сколько слотов АХ у бота сейчас: 10, пока аренда действует, иначе 5. Лимиты в `ah.rs`, `sell.rs`,
/// `listing.rs` берут число отсюда. Факт аренды подтягивается из файла при первом обращении.
pub fn slots(state: &Arc<Mutex<BotState>>) -> usize {
    let mut st = state.lock();
    ensure_loaded(&mut st);
    slots_at(st.rent_until_ms, now_ms())
}

/// Слотов при аренде до `until_ms` в момент `now_ms`.
pub fn slots_at(until_ms: u64, now_ms: u64) -> usize {
    if until_ms > now_ms {
        RENTED_SLOTS
    } else {
        BASE_SLOTS
    }
}

/// Сообщить оркестратору (а через него Go), до какого момента у бота арендованы слоты.
fn emit_status(username: &str, until_ms: u64) {
    ipc::emit(&ipc::WorkerMsg {
        name: "rent".into(),
        username: Some(username.to_string()),
        data: Some(serde_json::json!({ "until_ms": until_ms })),
        ..crate::empty_msg()
    });
}

/// Один раз за запуск: подтянуть факт аренды из файла (чтобы после перезапуска не платить повторно и сразу
/// знать, что слотов 10) и сказать об этом оркестратору.
fn ensure_loaded(st: &mut BotState) {
    if st.rent_loaded {
        return;
    }
    st.rent_loaded = true;
    // первая аренда после запуска — со случайной задержкой до 10 минут, чтобы боты не покупали слоты
    // одновременно (дальше они расходятся сами: каждый арендует ровно через час после своей прошлой)
    st.rent_retry_at = Instant::now()
        + Duration::from_secs(rand::Rng::gen_range(&mut rand::thread_rng(), 0..=600));
    if let Some(rec) = load_record_in(&store_dir(), &st.username) {
        st.rent_until_ms = rec.until_ms;
        st.rent_last_ms = rec.rented_at_ms;
        info!(
            "аренда → из файла: арендовано в {} (unix-мс), действует до {}; осталось {} мин",
            rec.rented_at_ms,
            rec.until_ms,
            rec.until_ms.saturating_sub(now_ms()) / 60_000
        );
        if rec.until_ms > now_ms() {
            emit_status(&st.username, rec.until_ms);
        }
    }
}

/// Пора ли арендовать: включено, час выгодный, не в паузе после неудачи, не чаще раза в `min_interval`
/// и прошлая аренда закончилась (или кончается через `renew_before`).
pub fn due(state: &Arc<Mutex<BotState>>) -> bool {
    let c = config();
    if !c.enabled {
        return false;
    }
    let mut st = state.lock();
    ensure_loaded(&mut st);
    due_at(c, st.rent_until_ms, st.rent_last_ms, st.rent_retry_at, st.balance, hour_msk_now(), now_ms(), Instant::now())
}

fn due_at(
    c: &RentConfig,
    until_ms: u64,
    last_ms: u64,
    retry_at: Instant,
    balance: Option<f64>,
    hour_msk: u32,
    now_ms: u64,
    now: Instant,
) -> bool {
    if !c.hour_allowed(hour_msk) {
        return false;
    }
    if now < retry_at {
        return false;
    }
    // денег не хватает на аренду (баланс неизвестен — не блокируем, сервер сам откажет)
    if balance.map_or(false, |b| b < c.price) {
        return false;
    }
    // не чаще раза в min_interval с прошлой аренды
    if last_ms > 0 && now_ms < last_ms + c.min_interval.as_millis() as u64 {
        return false;
    }
    // прошлая аренда ещё действует дольше, чем renew_before
    until_ms <= now_ms + c.renew_before.as_millis() as u64
}

#[derive(Debug)]
pub enum RentOutcome {
    Done,
    Failed(String),
}

/// Ждёт окно, которое показывает ожидаемый шаг и отличается от `old_id`. Возвращает его container id.
async fn wait_step(state: &Arc<Mutex<BotState>>, want: Step, old_id: i32) -> Result<i32, String> {
    let start = Instant::now();
    let mut last_title = String::new();
    while start.elapsed() < STEP_TIMEOUT {
        let (id, title) = {
            let s = state.lock();
            (s.last_window_id, s.last_window_title.clone())
        };
        last_title = title.clone();
        if id != old_id && id != 0 {
            match step_for_title(&title) {
                Some(step) if step == want => return Ok(id),
                Some(other) => {
                    return Err(format!(
                        "ждали «{}», открылось «{}» (заголовок «{title}»)",
                        want.name(),
                        other.name()
                    ));
                }
                None => {}
            }
        }
        delay::rnd("POLL").await;
    }
    Err(format!("окно «{}» не открылось за {} с, последний заголовок «{last_title}»", want.name(), STEP_TIMEOUT.as_secs()))
}

/// Вся цепочка аренды. Вызывать, когда бот на анархии и ничего другого с окнами не делает.
pub async fn run_rent(bot: &Client, state: &Arc<Mutex<BotState>>) -> RentOutcome {
    let username = state.lock().username.clone();
    let dir = store_dir();
    let prev = load_record_in(&dir, &username);
    let mut persisted = false;

    let res = run_rent_inner(bot, state, &username, &dir, &mut persisted).await;

    let mut s = state.lock();
    match &res {
        RentOutcome::Done => {
            s.rent_retry_at = Instant::now();
            s.rent_dump_pending = true;
            emit_status(&username, s.rent_until_ms);
            info!("аренда → слоты арендованы на {} мин", config().minutes);
        }
        RentOutcome::Failed(why) => {
            // запись делали до подтверждения; клик не удался, значит платы не было — откатываем
            if persisted {
                match prev {
                    Some(p) => {
                        let _ = save_record_in(&dir, &username, &p);
                        s.rent_until_ms = p.until_ms;
                        s.rent_last_ms = p.rented_at_ms;
                    }
                    None => {
                        delete_record_in(&dir, &username);
                        s.rent_until_ms = 0;
                        s.rent_last_ms = 0;
                    }
                }
            }
            emit_status(&username, s.rent_until_ms);
            s.rent_retry_at = Instant::now() + RETRY_BACKOFF;
            warn!("аренда → не вышло: {why}; повтор через {} мин", RETRY_BACKOFF.as_secs() / 60);
        }
    }
    drop(s);
    // что бы ни случилось, не оставляем открытым чужое окно
    let _ = safe_inv::try_close(bot);
    res
}

async fn run_rent_inner(
    bot: &Client,
    state: &Arc<Mutex<BotState>>,
    username: &str,
    dir: &std::path::Path,
    persisted: &mut bool,
) -> RentOutcome {
    // начинаем с закрытого окна, чтобы новый container id однозначно означал «открылось окно аренды»
    let _ = safe_inv::try_close(bot);
    delay::rnd("WINDOW_DELAY").await;

    let mut old_id = state.lock().last_window_id;
    delay::rnd("AH_CMD").await;
    crate::cmd::say(bot, "/ah rent");

    for step in [Step::Rent, Step::Currency, Step::Confirm] {
        let win_id = match wait_step(state, step, old_id).await {
            Ok(id) => id,
            Err(e) => return RentOutcome::Failed(e),
        };
        info!("аренда → окно «{}» (id {win_id}), клик по слоту {}", step.name(), step.slot());
        if step == Step::Confirm {
            // записываем факт аренды ДО клика по подтверждению: если процесс упадёт сразу после клика,
            // перезапущенный бот увидит запись и не заплатит второй раз
            let now = now_ms();
            let rec = RentRecord {
                rented_at_ms: now,
                until_ms: now + config().minutes * 60_000,
            };
            if let Err(e) = save_record_in(dir, username, &rec) {
                return RentOutcome::Failed(format!("не смог записать rent-state: {e} — не плачу, чтобы не арендовать дважды"));
            }
            *persisted = true;
            let mut s = state.lock();
            s.rent_until_ms = rec.until_ms;
            s.rent_last_ms = rec.rented_at_ms;
        }
        if step == Step::Confirm {
            let mut s = state.lock();
            s.rent_confirmed = false;
            s.rent_awaiting_confirm = true; // ставим до клика: быстрое сообщение не пропустим
        }
        // пауза перед кликом, как у человека; клик только если открыто то же самое окно
        delay::rnd("GLASS").await;
        if !safe_inv::try_left_click_expect(bot, step.slot(), win_id) {
            state.lock().rent_awaiting_confirm = false;
            return RentOutcome::Failed(format!(
                "окно «{}» (id {win_id}) исчезло до клика по слоту {}",
                step.name(),
                step.slot()
            ));
        }
        old_id = win_id;
    }

    // Ждём в чате «Успешная покупка!». Нет сообщения:
    //   * окно подтверждения всё ещё открыто → клик не прошёл, платы не было: откатываем запись и пробуем позже;
    //   * окно закрылось, но сообщения нет → исход неясен (например, не хватило денег). Запись не откатываем,
    //     чтобы не заплатить повторно; в худшем случае потеряем час аренды.
    let start = Instant::now();
    while start.elapsed() < CONFIRM_WAIT {
        if state.lock().rent_confirmed {
            state.lock().rent_awaiting_confirm = false;
            return RentOutcome::Done;
        }
        delay::rnd("POLL").await;
    }
    state.lock().rent_awaiting_confirm = false;
    let still_open = state.lock().last_window_id == old_id && safe_inv::try_id(bot) == Some(old_id);
    if still_open {
        return RentOutcome::Failed("окно подтверждения не закрылось и «Успешная покупка!» не пришло — клик не прошёл".into());
    }
    warn!("аренда → окно закрылось, но «Успешная покупка!» в чате не было: исход неясен, запись оставляю, чтобы не платить дважды");
    RentOutcome::Done
}

/// Строки чата про аренду пишем в лог, чтобы увидеть реальные сообщения («аренда закончилась», «не хватает
/// монет» и т. п.) и при необходимости научить бота им.
pub fn chat_hint(text: &str) -> bool {
    titles::title_has(text, "аренд")
}

/// «[V] Успешная покупка!» — сервер подтверждает, что аренда оплачена.
pub fn is_purchase_ok(text: &str) -> bool {
    titles::title_has(text, "успешная покупка")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(from: u32, to: u32) -> RentConfig {
        RentConfig {
            enabled: true,
            from_msk: from,
            to_msk: to,
            minutes: 60,
            renew_before: Duration::ZERO,
            min_interval: Duration::from_secs(3600),
            price: 1_500_000.0,
        }
    }

    #[test]
    fn titles_match_by_substring_any_case() {
        assert_eq!(step_for_title("Аренда слотов"), Some(Step::Rent));
        assert_eq!(step_for_title("АРЕНДА"), Some(Step::Rent));
        assert_eq!(step_for_title("  ✦ арендa слотов ✦ "), Some(Step::Rent)); // латинская «a»
        assert_eq!(step_for_title("Выберите валюту"), Some(Step::Currency));
        assert_eq!(step_for_title("ВЫБЕРИТЕ ВАЛЮТУ:"), Some(Step::Currency));
        assert_eq!(step_for_title("Подтверждение покупки"), Some(Step::Confirm));
        assert_eq!(step_for_title("подтверждение"), Some(Step::Confirm));
        assert_eq!(step_for_title("Confirm Accept"), Some(Step::Confirm));
        assert_eq!(step_for_title("Аукцион"), None);
        assert_eq!(step_for_title(""), None);
    }

    #[test]
    fn confirm_wins_when_it_mentions_rent() {
        // если окно подтверждения вдруг назовут «Подтверждение аренды» — это всё равно подтверждение
        assert_eq!(step_for_title("Подтверждение аренды"), Some(Step::Confirm));
    }

    #[test]
    fn latin_lookalikes_in_titles() {
        // «Пoдтверждeние пoкупки» с латинскими o и e
        assert_eq!(step_for_title("Выберите валюту"), Some(Step::Currency));
        assert_eq!(step_for_title("Пoдтверждeние пoкупки"), Some(Step::Confirm));
    }

    #[test]
    fn hour_window() {
        let c = cfg(10, 24);
        assert!(!c.hour_allowed(9));
        assert!(c.hour_allowed(10));
        assert!(c.hour_allowed(23));
        assert!(!c.hour_allowed(0));
        let wrap = cfg(22, 4);
        assert!(wrap.hour_allowed(23) && wrap.hour_allowed(0) && wrap.hour_allowed(3));
        assert!(!wrap.hour_allowed(4) && !wrap.hour_allowed(12));
        assert!(cfg(5, 5).hour_allowed(17)); // from == to → весь день
    }

    #[test]
    fn due_logic() {
        let c = cfg(10, 24);
        let now = Instant::now();
        let h = 3_600_000u64;
        let t = 1_000 * h; // «сейчас» в unix-мс
        // не арендовали ни разу, час выгодный → пора
        assert!(due_at(&c, 0, 0, now, None, 12, t, now));
        // час невыгодный → нет
        assert!(!due_at(&c, 0, 0, now, None, 3, t, now));
        // арендовали 10 минут назад на час → не пора
        assert!(!due_at(&c, t + 50 * 60_000, t - 10 * 60_000, now, None, 12, t, now));
        // аренда кончилась, и с прошлой прошёл час → пора
        assert!(due_at(&c, t - 1, t - h, now, None, 12, t, now));
        // аренда кончилась, но с прошлой прошло меньше часа (RENT_MINUTES < 60): не чаще раза в час
        let mut short = cfg(10, 24);
        short.minutes = 30;
        assert!(!due_at(&short, t - 1, t - 31 * 60_000, now, None, 12, t, now));
        // аренда кончилась в 24:00, а окно закрыто → не продлеваем
        assert!(!due_at(&c, t - 1, t - h, now, None, 0, t, now));
        // после неудачи — пауза
        assert!(!due_at(&c, 0, 0, now + Duration::from_secs(300), None, 12, t, now));
        // renew_before: за 2 минуты до конца уже можно (но не чаще раза в час)
        let mut early = cfg(10, 24);
        early.renew_before = Duration::from_secs(120);
        assert!(due_at(&early, t + 90_000, t - h, now, None, 12, t, now));
        assert!(!due_at(&early, t + 90_000, t - 30 * 60_000, now, None, 12, t, now));
    }

    #[test]
    fn record_survives_restart() {
        let dir = std::env::temp_dir().join(format!("rent-test-{}", now_ms()));
        let rec = RentRecord { rented_at_ms: 1_700_000_000_000, until_ms: 1_700_003_600_000 };
        assert_eq!(load_record_in(&dir, "nick1"), None);
        save_record_in(&dir, "nick1", &rec).unwrap();
        // «перезапуск»: читаем заново, как это делает новый процесс
        assert_eq!(load_record_in(&dir, "nick1"), Some(rec));
        // другой бот не видит чужую запись
        assert_eq!(load_record_in(&dir, "nick2"), None);
        // перезапись и удаление
        let rec2 = RentRecord { rented_at_ms: rec.until_ms, until_ms: rec.until_ms + 3_600_000 };
        save_record_in(&dir, "nick1", &rec2).unwrap();
        assert_eq!(load_record_in(&dir, "nick1"), Some(rec2));
        delete_record_in(&dir, "nick1");
        assert_eq!(load_record_in(&dir, "nick1"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_does_not_rent_again_within_the_hour() {
        // бот арендовал 20 минут назад, процесс перезапустили: из файла приходят until и rented_at
        let c = cfg(10, 24);
        let now = Instant::now();
        let t = 5_000u64 * 3_600_000;
        let rec = RentRecord { rented_at_ms: t - 20 * 60_000, until_ms: t + 40 * 60_000 };
        assert!(!due_at(&c, rec.until_ms, rec.rented_at_ms, now, None, 15, t, now));
        // а через 41 минуту (аренда кончилась, прошло >60 мин) — снова можно
        let later = t + 41 * 60_000;
        assert!(due_at(&c, rec.until_ms, rec.rented_at_ms, now, None, 15, later, now));
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(safe_name("Nick_1-x"), "Nick_1-x");
        assert_eq!(safe_name("../etc/pa ss"), "___etc_pa_ss");
    }

    #[test]
    fn slot_count_follows_rent() {
        let t = 5_000u64 * 3_600_000;
        assert_eq!(slots_at(0, t), BASE_SLOTS);
        assert_eq!(slots_at(t - 1, t), BASE_SLOTS); // аренда кончилась
        assert_eq!(slots_at(t + 1, t), RENTED_SLOTS);
        assert_eq!(slots_at(t + 3_600_000, t), 10);
    }

    #[test]
    fn not_enough_balance_blocks_rent() {
        let c = cfg(10, 24);
        let now = Instant::now();
        let t = 1_000u64 * 3_600_000;
        assert!(!due_at(&c, 0, 0, now, Some(1_000_000.0), 12, t, now)); // 1 млн < 1.5 млн
        assert!(due_at(&c, 0, 0, now, Some(14_000_000.0), 12, t, now));
        assert!(due_at(&c, 0, 0, now, None, 12, t, now)); // баланс неизвестен — не блокируем
    }

    #[test]
    fn purchase_ok_message() {
        assert!(is_purchase_ok("[V] Успешная покупка!"));
        assert!(is_purchase_ok("[V] успешная ПОКУПКА!"));
        assert!(is_purchase_ok("[V] Успeшнaя пoкупкa!")); // латинские двойники
        assert!(!is_purchase_ok("[☃] Вы успешно купили Незеритовый меч за $900,000!"));
        assert!(!is_purchase_ok("[☃] У Вас купили Незеритовый нагрудник"));
    }

    #[test]
    fn chat_hint_works() {
        assert!(chat_hint("[☃] Вы арендовали 5 слотов на 1 час"));
        assert!(chat_hint("АРЕНДА слотов закончилась"));
        assert!(!chat_hint("[☃] Вы успешно купили Незеритовый меч"));
    }
}
