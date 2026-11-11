use azalea_inventory::{ItemStack, Menu, SlotList};
use azalea_registry::builtin::ItemKind;

fn chest() -> Menu {
    Menu::Generic9x6 {
        contents: SlotList::default(),
        player: SlotList::default(),
    }
}

/// Запись TLauncher (01.11.2026): shift-click лота из окна 9x6 (слот 31) положил предмет в
/// слот 86 хотбара, когда 87–89 были заняты, то есть заполнение идёт с конца (ваниль).
#[test]
fn shift_click_from_chest_fills_from_the_end() {
    let mut m = chest();
    *m.slot_mut(31).unwrap() = ItemStack::new(ItemKind::NetheriteHelmet, 1);
    for s in [87, 88, 89] {
        *m.slot_mut(s).unwrap() = ItemStack::new(ItemKind::NetheriteBoots, 1);
    }
    m.quick_move_stack(31);
    assert!(m.slot(31).unwrap().is_empty());
    assert!(m.slot(86).unwrap().is_present(), "ожидали слот 86");
    assert!(m.slot(54).unwrap().is_empty(), "первый слот инвентаря не должен занимать");
}

#[test]
fn shift_click_into_empty_player_goes_to_last_slot() {
    let mut m = chest();
    *m.slot_mut(10).unwrap() = ItemStack::new(ItemKind::NetheriteHelmet, 1);
    m.quick_move_stack(10);
    assert!(m.slot(89).unwrap().is_present());
}
