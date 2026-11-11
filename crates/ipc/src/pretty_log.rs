//! Логи как в 4narek-old: `[nick] ✓ …` без timestamp/target.
//! Azalea/bevy packet-шум выключен в дефолтном фильтре — без RUST_LOG.
//!
//! **Казначей** — глобальный флаг `IS_TREASURER` (AtomicBool). Когда
//! воркер получает `OrchCmd::TreasurerRole` и становится казначеем —
//! вызывается `set_treasurer(true)`. Тогда **все** логи (`info!`,
//! `warn!`, `line`, `line_ex`) для этого процесса красятся в magenta.
//! `set_treasurer(false)` — обратно в обычный цвет.
//!
//! `line_ex(..., is_treasurer)` — для орка, где казначейство
//! **чужое** (орк логирует **другого** бота). Если `is_treasurer == true`
//! **или** глобальный `IS_TREASURER == true` — magenta.

use std::{
    fmt,
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
};

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{
    EnvFilter,
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

static TAG: OnceLock<String> = OnceLock::new();
/// Глобальный флаг «этот процесс — казначей». Выставляется из
/// `worker main.rs` / `state.rs` при `OrchCmd::TreasurerRole`.
/// Влияет на **все** логи: `info!`, `warn!`, `line`, `line_ex`.
static IS_TREASURER: AtomicBool = AtomicBool::new(false);

pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
pub const BOLD: &str = "\x1b[1m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";
pub const MAGENTA: &str = "\x1b[35m";

/// Выставить глобальный флаг «я казначей». Воркер зовёт при
/// `OrchCmd::TreasurerRole { username }`, где `username == boot.username`.
/// `false` — снять (я больше не казначей).
pub fn set_treasurer(v: bool) {
    IS_TREASURER.store(v, Ordering::Relaxed);
}

/// Узнать, казначей ли этот процесс (для отладки / проверок).
pub fn is_treasurer() -> bool {
    IS_TREASURER.load(Ordering::Relaxed)
}

/// Строка покупки: id · цена · прочность% (без энчантов).
pub fn ah_buy_line(verb: &str, id: &str, price_s: &str, dur_pct: Option<i32>) -> String {
    let dur = dur_pct
        .map(|d| format!(" {DIM}·{RESET} {CYAN}{d}%{RESET}"))
        .unwrap_or_default();
    format!(
        "{GREEN}{BOLD}АХ → {verb}{RESET} {BOLD}{id}{RESET} {DIM}·{RESET} {YELLOW}{price_s}{RESET}{dur}"
    )
}

fn default_filter() -> EnvFilter {
    EnvFilter::new(concat!(
        "info,",
        "azalea=off,",
        "azalea_client=off,",
        "azalea_core=off,",
        "azalea_buf=off,",
        "azalea_protocol=off,",
        "azalea_world=off,",
        "azalea_entity=off,",
        "azalea_physics=off,",
        "azalea_inventory=off,",
        "azalea_auth=off,",
        "azalea_crypto=off,",
        "azalea_chat=off,",
        "azalea_block=off,",
        "azalea_brigadier=off,",
        "bevy=off,",
        "bevy_ecs=off,",
        "bevy_app=off,",
        "bevy_log=off,",
        "bevy_time=off,",
        "bevy_reflect=off,",
        "bevy_utils=off,",
        "hyper=off,",
        "reqwest=off,",
        "tungstenite=off,",
        "tokio_tungstenite=off,",
        "hickory_proto=off,",
        "hickory_resolver=off,",
        "mio=off,",
        "want=off",
    ))
}

struct OldStyle;

impl<S, N> FormatEvent<S, N> for OldStyle
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let tag = TAG.get().map(|s| s.as_str()).unwrap_or("bot");
        let level = *event.metadata().level();
        let is_treasurer = IS_TREASURER.load(Ordering::Relaxed);

        // Казначей: тег magenta + ★ вместо ℹ (для Info).
        let tag_color = if is_treasurer { MAGENTA } else { CYAN };
        write!(writer, "{tag_color}[{tag}]{RESET} ")?;

        match level {
            Level::ERROR => write!(writer, "{RED}✖{RESET} {RED}")?,
            Level::WARN => write!(writer, "{YELLOW}⚡{RESET} ")?,
            Level::INFO => {
                if is_treasurer {
                    write!(writer, "{BOLD}★{RESET} ")?
                } else {
                    write!(writer, "{BOLD}ℹ{RESET} ")?
                }
            }
            Level::DEBUG | Level::TRACE => write!(writer, "{DIM}·{RESET} {DIM}")?,
        }

        ctx.format_fields(writer.by_ref(), event)?;
        if matches!(level, Level::ERROR | Level::DEBUG | Level::TRACE) {
            write!(writer, "{RESET}")?;
        }
        writeln!(writer)
    }
}

/// Воркер: тег = ник. Звать после чтения BotBoot.
/// Сбрасывает `IS_TREASURER` в `false` — стартуем как обычный.
pub fn init_bot(username: &str) {
    let _ = TAG.set(username.to_string());
    IS_TREASURER.store(false, Ordering::Relaxed);
    init_inner();
}

/// Оркестратор.
pub fn init_orch() {
    let _ = TAG.set("orch".into());
    IS_TREASURER.store(false, Ordering::Relaxed);
    init_inner();
}

fn init_inner() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .event_format(OldStyle)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Info-строка с готовой ANSI-раскраской внутри (`ah_buy_line`). Через `info!` нельзя:
/// tracing-subscriber ≥0.3.20 экранирует ESC в сообщениях и в консоли печатается буквальное `\x1b[32m`.
pub fn info_ansi(msg: &str) {
    let nick = TAG.get().map(|s| s.as_str()).unwrap_or("bot");
    line(nick, LineKind::Info, msg);
}

/// Прямой println в стиле old (IPC alerts / orch про бота).
///
/// Обёртка над `line_ex(..., false)` — для обратной совместимости.
pub fn line(nick: &str, kind: LineKind, msg: &str) {
    line_ex(nick, kind, msg, false);
}

/// То же, что `line`, но с флагом «это казначей».
///
/// `is_treasurer == true` **или** глобальный `IS_TREASURER == true` —
/// строка красится в magenta (кроме Chat/Afk — они остаются как есть).
///
/// **Орк** использует `is_treasurer` явно (он знает, кто казначей).
/// **Воркер** полагается на глобальный флаг (`set_treasurer`).
pub fn line_ex(nick: &str, kind: LineKind, msg: &str, is_treasurer: bool) {
    let mut out = std::io::stderr().lock();
    let is_treasurer = is_treasurer || IS_TREASURER.load(Ordering::Relaxed);

    if is_treasurer {
        let _ = match kind {
            LineKind::Info => {
                writeln!(out, "{MAGENTA}[{nick}]{RESET} {BOLD}★{RESET} {msg}")
            }
            LineKind::Ok => {
                writeln!(out, "{MAGENTA}[{nick}]{RESET} {GREEN}✓{RESET} {msg}")
            }
            LineKind::Warn => {
                writeln!(out, "{MAGENTA}[{nick}]{RESET} {YELLOW}⚡{RESET} {msg}")
            }
            LineKind::Err => {
                writeln!(
                    out,
                    "{MAGENTA}[{nick}]{RESET} {RED}✖{RESET} {RED}{msg}{RESET}"
                )
            }
            LineKind::Chat => {
                writeln!(out, "{DIM}{MAGENTA}[{nick}]{RESET}{DIM} 💬 {msg}{RESET}")
            }
            LineKind::Afk => {
                writeln!(
                    out,
                    "{MAGENTA}[{nick}]{RESET} {RED}{BOLD}AFK{RESET} {RED}{msg}{RESET}"
                )
            }
            LineKind::Treasurer => {
                writeln!(out, "{MAGENTA}[{nick}]{RESET} {BOLD}★{RESET} {msg}")
            }
        };
        return;
    }

    let _ = match kind {
        LineKind::Info => writeln!(out, "{CYAN}[{nick}]{RESET} {BOLD}ℹ{RESET} {msg}"),
        LineKind::Ok => writeln!(out, "{CYAN}[{nick}]{RESET} {GREEN}✓{RESET} {msg}"),
        LineKind::Warn => writeln!(out, "{CYAN}[{nick}]{RESET} {YELLOW}⚡{RESET} {msg}"),
        LineKind::Err => writeln!(out, "{CYAN}[{nick}]{RESET} {RED}✖{RESET} {RED}{msg}{RESET}"),
        LineKind::Chat => writeln!(out, "{DIM}{CYAN}[{nick}]{RESET}{DIM} 💬 {msg}{RESET}"),
        LineKind::Afk => {
            writeln!(
                out,
                "{CYAN}[{nick}]{RESET} {RED}{BOLD}AFK{RESET} {RED}{msg}{RESET}"
            )
        }
        LineKind::Treasurer => writeln!(out, "{CYAN}[{nick}]{RESET} {BOLD}ℹ{RESET} {msg}"),
    };
}

#[derive(Clone, Copy)]
pub enum LineKind {
    Info,
    Ok,
    Warn,
    Err,
    Chat,
    Afk,
    /// Казначей — magenta + ★. Использовать через `line_ex(..., true)`
    /// или полагаться на глобальный `IS_TREASURER`.
    Treasurer,
}