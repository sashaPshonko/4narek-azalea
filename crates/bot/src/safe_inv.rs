//! Inventory без panic (как `try_y` для Position).
//! Azalea `get_inventory()` / `query_self<&Inventory>` падают, если компонента нет
//! (transfer / respawn / mid-GUI).

use azalea::container::ContainerHandleRef;
use azalea::entity::inventory::Inventory;
use azalea::prelude::*;
use azalea_inventory::{ItemStack, Menu};

pub fn ready(bot: &Client) -> bool {
    bot.get_component::<Inventory>().is_some()
}

pub fn try_handle(bot: &Client) -> Option<ContainerHandleRef> {
    bot.get_component::<Inventory>()
        .map(|inv| ContainerHandleRef::new(inv.id, bot.clone()))
}

pub fn try_id(bot: &Client) -> Option<i32> {
    bot.get_component::<Inventory>().map(|inv| inv.id)
}

/// Закрыть открытое окно, если Inventory жив. `true` = отправили close.
pub fn try_close(bot: &Client) -> bool {
    let Some(inv) = bot.get_component::<Inventory>() else {
        return false;
    };
    if inv.id == 0 {
        return false;
    }
    ContainerHandleRef::new(inv.id, bot.clone()).close();
    true
}

/// Слоты контейнера без инвентаря игрока — как `ContainerHandleRef::contents`.
pub fn try_contents(bot: &Client) -> Option<Vec<ItemStack>> {
    let inv = bot.get_component::<Inventory>()?;
    if inv.id == 0 {
        return None;
    }
    Some(inv.container_menu.as_ref()?.contents())
}

/// Активное меню целиком (контейнер или player).
pub fn try_menu(bot: &Client) -> Option<Menu> {
    let inv = bot.get_component::<Inventory>()?;
    if inv.id == 0 {
        Some(inv.inventory_menu.clone())
    } else {
        inv.container_menu.clone()
    }
}

pub fn try_player_slots(bot: &Client) -> Option<Vec<ItemStack>> {
    bot.get_component::<Inventory>()
        .map(|inv| inv.inventory_menu.slots())
}

/// Клики безопасны (не трогают expect в menu); нужен живой Inventory только для id.
pub fn try_left_click(bot: &Client, slot: usize) -> bool {
    let Some(h) = try_handle(bot) else {
        return false;
    };
    if h.id() == 0 {
        return false;
    }
    h.left_click(slot);
    true
}

pub fn try_shift_click(bot: &Client, slot: usize) -> bool {
    let Some(h) = try_handle(bot) else {
        return false;
    };
    if h.id() == 0 {
        return false;
    }
    h.shift_click(slot);
    true
}
