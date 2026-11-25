//! Вход на сервер авторизации FunTime: `/reg` и `/l` только в ответ на подсказку сервера, как у человека.
//!
//! Раньше бот на каждый Spawn (вход, /hub, /an, смена мира) слепо слал `/reg <пароль>` и `/l <пароль>`
//! в один тик. Сервер отвечал «Вы уже авторизованы» по два раза за заход, а живой игрок так не делает.
//!
//! Тексты сервера (из логов фермы):
//! - `[✾] Зарегистрируйтесь ↝ /reg <Пароль>` → `/reg <пароль>` (после неё уже вошёл: «Успешная регистрация!»);
//! - `[✾] Войдите в игру ↝ /login <Пароль>` и `[✾] Сначала авторизируйтесь` → `/l <пароль>`;
//! - `[✾] Успешная авторизация!`, `[✾] Успешная регистрация!`, `[✾] Вы уже авторизованы…` → вошли.
//!
//! Сервер повторяет подсказку, пока не войдёшь, поэтому отвечаем не чаще раза в [`RESEND_AFTER`].
//! Пока решается капча, подсказку запоминаем и отвечаем на неё после капчи.

use std::time::{Duration, Instant};

use azalea::prelude::*;
use parking_lot::Mutex;
use rand::Rng;
use tracing::{info, warn};

/// Не повторять ответ на подсказку раньше этого (сервер шлёт её повторно, пока не войдёшь).
const RESEND_AFTER: Duration = Duration::from_secs(7);
/// Если после входа в мир подсказки так и не было, а «вошли» не видно — через столько шлём `/l` сами.
const FALLBACK_AFTER: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompt {
    Register,
    Login,
}

#[derive(Default)]
struct Auth {
    /// Сервер подтвердил вход в этом подключении.
    authed: bool,
    /// Когда последний раз ответили на подсказку.
    last_sent: Option<Instant>,
    /// Подсказка пришла во время капчи: ответить после неё.
    pending: Option<Prompt>,
    /// Подсказку в этом подключении видели (для запасного `/l`).
    prompt_seen: bool,
    /// Номер подключения: запасной `/l` из старого подключения не должен сработать в новом.
    conn: u64,
}

static AUTH: Mutex<Auth> = Mutex::new(Auth {
    authed: false,
    last_sent: None,
    pending: None,
    prompt_seen: false,
    conn: 0,
});

/// Подсказка сервера авторизации. Только системные строки с `[✾]`, чтобы чат игроков не дёргал вход.
pub fn classify(text: &str) -> Option<Prompt> {
    let t = text.trim_start();
    if !t.starts_with("[✾]") {
        return None;
    }
    let low = t.to_lowercase();
    if low.contains("зарегистрируйтесь") || t.contains("/reg <") {
        Some(Prompt::Register)
    } else if low.contains("войдите в игру") || low.contains("авторизируйтесь") || t.contains("/login <") {
        Some(Prompt::Login)
    } else {
        None
    }
}

/// Сервер сообщил, что вход выполнен.
pub fn is_authed_text(text: &str) -> bool {
    let t = text.trim_start();
    if !t.starts_with("[✾]") {
        return false;
    }
    let low = t.to_lowercase();
    low.contains("успешная авторизация") || low.contains("успешная регистрация") || low.contains("уже авторизованы")
}

/// Новое подключение (Init): начинаем с нуля.
pub fn on_new_connection() {
    let mut a = AUTH.lock();
    let conn = a.conn + 1;
    *a = Auth { conn, ..Auth::default() };
}

pub fn on_authed() {
    let mut a = AUTH.lock();
    if !a.authed {
        info!("авторизация → вошли");
    }
    a.authed = true;
    a.pending = None;
}

/// Ответ на подсказку сервера. `captcha_busy` — сейчас решается капча (ответим после неё).
pub fn on_prompt(bot: &Client, kind: Prompt, password: &str, captcha_busy: bool) {
    let mut a = AUTH.lock();
    a.prompt_seen = true;
    if a.authed {
        return;
    }
    if captcha_busy {
        a.pending = Some(kind);
        return;
    }
    let now = Instant::now();
    if a.last_sent.is_some_and(|t| now.duration_since(t) < RESEND_AFTER) {
        return;
    }
    a.last_sent = Some(now);
    let conn = a.conn;
    drop(a);
    send_like_human(bot.clone(), kind, password.to_string(), conn);
}

/// Капча решена: если подсказка пришла во время капчи — отвечаем на неё сейчас.
pub fn after_captcha(bot: &Client, password: &str) {
    let pending = AUTH.lock().pending.take();
    if let Some(kind) = pending {
        on_prompt(bot, kind, password, false);
    }
}

/// Бот вошёл в мир. Если подсказки нет и вход не подтверждён — через [`FALLBACK_AFTER`] шлём `/l` сами
/// (на случай, если сервер сменит текст подсказки). На уже авторизованном подключении ничего не делает.
pub fn on_spawn(bot: &Client, password: &str) {
    let conn = {
        let a = AUTH.lock();
        if a.authed {
            return;
        }
        a.conn
    };
    let bot = bot.clone();
    let password = password.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(FALLBACK_AFTER);
        let mut a = AUTH.lock();
        if a.conn != conn || a.authed || a.prompt_seen || a.pending.is_some() {
            return;
        }
        if crate::boot::global().state.lock().captcha_busy {
            return;
        }
        a.last_sent = Some(Instant::now());
        drop(a);
        warn!("авторизация → подсказки сервера нет {}с, шлю /l сам", FALLBACK_AFTER.as_secs());
        crate::cmd::say(&bot, format!("/l {password}"));
    });
}

/// Пауза «прочитал подсказку и набрал команду с паролем», потом команда.
fn send_like_human(bot: Client, kind: Prompt, password: String, conn: u64) {
    let wait = {
        let mut rng = rand::thread_rng();
        // `/l пароль` — около 10–15 символов, `/reg пароль` чуть длиннее
        let ms = match kind {
            Prompt::Login => rng.gen_range(1300..=2900),
            Prompt::Register => rng.gen_range(1800..=3600),
        };
        Duration::from_millis(ms)
    };
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        {
            let a = AUTH.lock();
            if a.conn != conn || a.authed {
                return;
            }
        }
        match kind {
            Prompt::Register => crate::cmd::say(&bot, format!("/reg {password}")),
            Prompt::Login => crate::cmd::say(&bot, format!("/l {password}")),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_from_server() {
        assert_eq!(classify("[✾] Зарегистрируйтесь ↝ /reg <Пароль>"), Some(Prompt::Register));
        assert_eq!(classify("[✾] Войдите в игру ↝ /login <Пароль>"), Some(Prompt::Login));
        assert_eq!(classify("[✾] Сначала авторизируйтесь"), Some(Prompt::Login));
        assert_eq!(classify("[✾] Успешная авторизация! Приятной игры!"), None);
        assert_eq!(classify("[✾] Вы уже авторизованы, вводить пароль не нужно!"), None);
    }

    #[test]
    fn player_chat_does_not_trigger() {
        assert_eq!(classify("Ⓛ <Swork> [Игрок] someone ⇨ напиши /reg <Пароль>"), None);
        assert_eq!(classify("Ⓛ [Игрок] x ⇨ войдите в игру"), None);
        assert!(!is_authed_text("Ⓛ [Игрок] x ⇨ успешная авторизация"));
    }

    #[test]
    fn authed_texts() {
        assert!(is_authed_text("[✾] Успешная авторизация! Приятной игры!"));
        assert!(is_authed_text("[✾] Успешная регистрация! Приятной игры!"));
        assert!(is_authed_text("[✾] Вы уже авторизованы, вводить пароль не нужно!"));
    }
}
