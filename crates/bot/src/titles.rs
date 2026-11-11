//! Единое место, где по заголовку окна узнаётся, что это за окно. Раньше одни и те же подстроки были
//! записаны в нескольких местах `ah.rs` и `rent.rs`, и правка в одном месте теряла другое.
//!
//! Все проверки — **по подстроке, без учёта регистра** и с поправкой на латинские буквы-двойники
//! (`a`/`о`, `e`/`е`, `p`/`р`, …), которые FunTime подмешивает в русские слова («Ценa», «Истeкaeт»).

/// Нормализация: нижний регистр + латинские двойники → кириллица.
pub fn norm(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'a' => 'а',
            'e' => 'е',
            'o' => 'о',
            'p' => 'р',
            'c' => 'с',
            'x' => 'х',
            'y' => 'у',
            'k' => 'к',
            'm' => 'м',
            't' => 'т',
            'h' => 'н',
            'b' => 'в',
            _ => c,
        })
        .collect()
}

/// Текст содержит подстроку (без регистра, с учётом латинских двойников).
pub fn title_has(title: &str, needle: &str) -> bool {
    norm(title).contains(&norm(needle))
}

fn has_any(title: &str, needles: &[&str]) -> bool {
    let t = norm(title);
    needles.iter().any(|n| t.contains(&norm(n)))
}

/// «Подтверждение покупки» (и английское accept).
pub fn is_confirm(title: &str) -> bool {
    has_any(title, &["подтверж", "подтвердить", "accept", GLYPH_CONFIRM])
}

/// Заголовки окон аренды на сервере приходят не словами, а шрифтовыми значками (шрифт `minecraft:auction` /
/// `minecraft:ts`), поэтому по слову «аренда» их не узнать. Сняты с живого клиента 07.11.2026 (`/ah rent`):
/// окно аренды — `0A1А1G2z2s`, окно подтверждения покупки ([Купить]/[Отменить]) — `0B1E2z2L`.
const GLYPH_RENT: &str = "0A1А1G2z2s";
const GLYPH_CONFIRM: &str = "0B1E2z2L";

/// «Хранилище» / «Мои товары».
pub fn is_storage(title: &str) -> bool {
    has_any(title, &["хранилищ", "мои товар"])
}

/// Окно RTP / «случайная точка».
pub fn is_rtp(title: &str) -> bool {
    has_any(title, &["rtp", "ртп", "случай"])
}

/// Окно аренды слотов (`/ah rent`).
pub fn is_rent(title: &str) -> bool {
    title_has(title, "аренд") || title_has(title, GLYPH_RENT)
}

/// «Выберите валюту» в цепочке аренды.
pub fn is_currency(title: &str) -> bool {
    title_has(title, "валют")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rent_and_confirm_glyph_titles() {
        // заголовки с живого сервера: слов в них нет, только шрифтовые значки
        assert!(is_rent("0A1А1G2z2s"));
        assert!(is_confirm("0B1E2z2L"));
        assert!(!is_rent("0B1E2z2L"));
        assert!(!is_confirm("0A1А1G2z2s"));
    }

    #[test]
    fn confirm_any_case_and_lookalikes() {
        assert!(is_confirm("Подтверждение покупки"));
        assert!(is_confirm("ПОДТВЕРДИТЬ"));
        assert!(is_confirm("Пoдтверждeние")); // латинские o и e
        assert!(is_confirm("Accept"));
        assert!(!is_confirm("Аукцион"));
    }

    #[test]
    fn storage_and_rtp() {
        assert!(is_storage("0A2zspace.-1091/487space.-87   Хранилище"));
        assert!(is_storage("Мои товары"));
        assert!(!is_storage("漢:netherite"));
        assert!(is_rtp("RTP"));
        assert!(is_rtp("Ртп"));
        assert!(is_rtp("Случайная точка"));
        assert!(!is_rtp("Хранилище"));
    }

    #[test]
    fn rent_and_currency() {
        assert!(is_rent("Аренда слотов"));
        assert!(is_rent("арендa")); // латинская «a»
        assert!(is_currency("Выберите валюту"));
        assert!(!is_rent("Выберите валюту"));
        assert!(!is_currency("Аренда слотов"));
    }
}
