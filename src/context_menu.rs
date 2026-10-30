// SPDX-License-Identifier: MIT
//! The right-click menu: copy selected text, and cut, paste and clear in a
//! text field. egui ends a selection, and a field's focus, on any
//! press, so this takes every right-click before egui sees it, as well as the
//! clicks on its own menu, and acts through egui's clipboard events.

use eframe::egui::{self, Event, Id, Key, Modifiers, PointerButton, Pos2, Rect};

#[derive(Default)]
pub struct ContextMenu {
    /// A right-click this pass, to open the menu at.
    open_at: Option<Pos2>,
    menu: Option<Menu>,
    /// Where the menu was drawn, to take the clicks on it.
    rect: Option<Rect>,
    /// A click on the menu, taken from egui, and its release still to take.
    click: Option<Pos2>,
    take_release: bool,
    /// The text field under the pointer this pass.
    field: Option<Field>,
    /// Sent to egui with the next pass's input.
    events: Vec<Event>,
}

struct Field {
    id: Id,
    selected: bool,
    empty: bool,
}

struct Menu {
    pos: Pos2,
    items: Vec<Item>,
}

#[derive(Clone, Copy, PartialEq)]
enum Item {
    Cut,
    Copy,
    Paste,
    Clear,
}

/// Registers a text field for the menu; call it after adding the field.
pub fn field(ui: &egui::Ui, r: &egui::Response, text: &str) {
    if !(r.enabled() && r.contains_pointer()) {
        return;
    }
    // The field shows its stored selection only while it has the focus.
    let range = egui::TextEdit::load_state(ui.ctx(), r.id).and_then(|s| s.cursor.char_range());
    let selected = r.has_focus() && range.is_some_and(|r| !r.is_empty());
    ui.ctx().plugin::<ContextMenu>().lock().field = Some(Field { id: r.id, selected, empty: text.is_empty() });
}

/// Shows the menu while it is open; call it at the end of each pass. Not
/// from the plugin's own end of pass: in debug builds egui tells every plugin
/// about the widget under the pointer, so drawing while locked deadlocks.
pub fn show(ctx: &egui::Context) {
    let handle = ctx.plugin::<ContextMenu>();
    let (menu, click) = {
        let mut m = handle.lock();
        if let Some(pos) = m.open_at.take() {
            let items = m.items(ctx);
            m.menu = (!items.is_empty()).then_some(Menu { pos, items });
        }
        let Some(menu) = &m.menu else {
            m.rect = None;
            return;
        };
        ((menu.pos, menu.items.clone()), m.click.take())
    };
    let ((pos, items), mut chosen) = (menu, None);
    let area = egui::Area::new(Id::unique("context-menu")).order(egui::Order::Foreground).fixed_pos(pos).constrain(true);
    let r = area.show(ctx, |ui| {
        egui::Frame::menu(ui.style()).show(ui, |ui| {
            ui.set_min_width(140.0);
            for item in items {
                let (text, keys) = ContextMenu::label(ctx, item);
                let b = egui::Button::new(text).shortcut_text(keys).frame_when_inactive(false);
                let r = ui.add_sized([ui.available_width(), ui.spacing().interact_size.y], b);
                if click.is_some_and(|p| r.rect.contains(p)) {
                    chosen = Some(item);
                }
            }
        });
    });
    let mut m = handle.lock();
    m.rect = Some(r.response.rect);
    if let Some(item) = chosen {
        m.menu = None;
        m.act(ctx, item);
    }
}

impl ContextMenu {
    fn items(&mut self, ctx: &egui::Context) -> Vec<Item> {
        if let Some(f) = self.field.take() {
            // Paste goes where the menu was opened.
            ctx.memory_mut(|m| m.request_focus(f.id));
            let mut items = Vec::new();
            if f.selected {
                items.extend([Item::Cut, Item::Copy]);
            }
            items.push(Item::Paste);
            if !f.empty {
                items.push(Item::Clear);
            }
            return items;
        }
        if ctx.plugin::<egui::text_selection::LabelSelectionState>().lock().has_selection() {
            return vec![Item::Copy];
        }
        Vec::new()
    }

    fn act(&mut self, ctx: &egui::Context, item: Item) {
        match item {
            Item::Cut => self.events.push(Event::Cut),
            Item::Copy => self.events.push(Event::Copy),
            Item::Paste => ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste),
            Item::Clear => {
                let key = |key, modifiers| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
                self.events.extend([key(Key::A, Modifiers::COMMAND), key(Key::Delete, Modifiers::NONE)]);
            }
        }
        ctx.request_repaint();
    }

    fn label(ctx: &egui::Context, item: Item) -> (&'static str, String) {
        let keys = |key| ctx.format_shortcut(&egui::KeyboardShortcut::new(Modifiers::COMMAND, key));
        match item {
            Item::Cut => ("Cut", keys(Key::X)),
            Item::Copy => ("Copy", keys(Key::C)),
            Item::Paste => ("Paste", keys(Key::V)),
            Item::Clear => ("Clear", String::new()),
        }
    }
}

impl egui::Plugin for ContextMenu {
    fn debug_name(&self) -> &'static str {
        "context menu"
    }

    fn on_begin_pass(&mut self, _ui: &mut egui::Ui) {
        self.field = None;
    }

    fn input_hook(&mut self, _ctx: &egui::Context, input: &mut egui::RawInput) {
        input.events.append(&mut self.events);
        input.events.retain(|e| match *e {
            Event::PointerButton { pos, button: PointerButton::Secondary, pressed, .. } => {
                if pressed {
                    self.open_at = Some(pos);
                    self.menu = None;
                }
                false
            }
            Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, .. } if self.menu.is_some() => {
                if self.rect.is_some_and(|r| r.contains(pos)) {
                    self.click = Some(pos);
                    self.take_release = true;
                    return false;
                }
                self.menu = None;
                true
            }
            Event::PointerButton { button: PointerButton::Primary, pressed: false, .. } if self.take_release => {
                self.take_release = false;
                false
            }
            Event::Key { key: Key::Escape, pressed: true, .. } if self.menu.is_some() => {
                self.menu = None;
                false
            }
            Event::MouseWheel { .. } | Event::WindowFocused(false) => {
                self.menu = None;
                true
            }
            _ => true,
        });
    }
}
