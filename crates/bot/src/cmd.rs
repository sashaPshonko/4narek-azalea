//! Отправка чата с логом: видно каждую команду бота.

use azalea::prelude::*;
use tracing::info;

pub fn say(bot: &Client, text: impl AsRef<str>) {
    let text = text.as_ref();
    info!("→ {text}");
    bot.chat(text.to_string());
}
