//! Отправка чата с логом: видно каждую команду бота.

use azalea::prelude::*;
use tracing::info;

pub fn say(bot: &Client, text: impl AsRef<str>) {
    let text = text.as_ref();
    if crate::health::is_dead() {
        // Воркер объявлен мёртвым (связь потеряна): в чат не пишем, процесс скоро перезапустится.
        return;
    }
    info!("→ {text}");
    bot.chat(text.to_string());
}

/// `/pay` с проверкой по табу: ник получателя должен быть онлайн прямо сейчас, иначе команду не шлём
/// (перевод несуществующему нику и палит, и не проходит). Проверка перед КАЖДОЙ командой, в том числе
/// перед второй (подтверждением). true — команда отправлена.
pub fn pay(bot: &Client, to: &str, amount: i64) -> bool {
    if bot.player_uuid_by_username(to).is_none() {
        info!("/pay {to}: нет в табе — команду не шлю");
        return false;
    }
    say(bot, format!("/pay {to} {amount}"));
    true
}
