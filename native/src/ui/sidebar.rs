// SPDX-License-Identifier: GPL-3.0-or-later

//! The category rail: product mark, status card, category rows with derived
//! counts, and Settings at the bottom.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

use warcraft_rustcorder::coordinator::AppSnapshot;
use warcraft_rustcorder::domain::Category;

use super::status::StatusCard;
use super::{ActionSink, CATEGORIES, ShellAction, icon_button};

/// One category row as the rail renders it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowView {
    pub category: Category,
    pub count: usize,
    pub visible: bool,
    pub active: bool,
}

/// `hide_empty_categories` hides only zero-entry categories, only once the
/// library holds any video; Manual stays visible while manual recording is on.
pub fn rows(snapshot: &AppSnapshot) -> Vec<RowView> {
    let total: usize = snapshot
        .category_counts
        .iter()
        .map(|(_, count)| count)
        .sum();
    CATEGORIES
        .iter()
        .map(|(category, _, _)| {
            let count = snapshot
                .category_counts
                .iter()
                .find(|(candidate, _)| candidate == category)
                .map_or(0, |(_, count)| *count);
            let force_show = *category == Category::Manual && snapshot.config.manual.enabled;
            let visible = !snapshot.config.interface.hide_empty_categories
                || total == 0
                || count > 0
                || force_show;
            RowView {
                category: category.clone(),
                count,
                visible,
                active: snapshot.config.interface.selected_category == *category,
            }
        })
        .collect()
}

pub struct Sidebar {
    pub widget: gtk4::Box,
    pub status_card: StatusCard,
    list: gtk4::ListBox,
    rows: Vec<(Category, gtk4::ListBoxRow, gtk4::Label)>,
    /// The snapshot's selected category, as last applied.
    current: Rc<RefCell<Option<Category>>>,
    settings_warning: gtk4::Image,
}

impl Sidebar {
    pub fn new(sink: ActionSink) -> Self {
        let widget = gtk4::Box::new(gtk4::Orientation::Vertical, 6);

        let mark = gtk4::Image::from_icon_name("warcraft-rustcorder");
        mark.set_pixel_size(32);
        mark.set_valign(gtk4::Align::Center);
        let name = gtk4::Label::new(Some("Warcraft Rustcorder"));
        name.add_css_class("title-3");
        name.set_xalign(0.0);
        let brand = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        brand.set_margin_top(12);
        brand.set_margin_start(12);
        brand.set_margin_end(12);
        brand.append(&mark);
        brand.append(&name);
        widget.append(&brand);

        let status_card = StatusCard::new(Rc::clone(&sink));
        widget.append(&status_card.widget);

        let list = gtk4::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk4::SelectionMode::Single);
        list.set_vexpand(true);
        list.set_margin_start(6);
        list.set_margin_end(6);

        let mut rows = Vec::new();
        for (category, label, icon_name) in CATEGORIES {
            let icon = gtk4::Image::from_icon_name(icon_name);
            icon.set_pixel_size(20);
            let text = gtk4::Label::new(Some(label));
            text.set_xalign(0.0);
            text.set_hexpand(true);
            let count = gtk4::Label::new(Some("0"));
            count.add_css_class("category-count");
            let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row_box.set_margin_top(4);
            row_box.set_margin_bottom(4);
            row_box.set_margin_start(6);
            row_box.set_margin_end(6);
            row_box.append(&icon);
            row_box.append(&text);
            row_box.append(&count);
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&row_box));
            row.set_tooltip_text(Some(label));
            list.append(&row);
            rows.push((category, row, count));
        }

        // Selection, not activation: arrow keys move the selection without
        // activating. `apply` selects the snapshot's category programmatically,
        // which is recognised here as the current category and not re-sent.
        // Before the first snapshot GTK auto-selects the first row; ignoring
        // that keeps it from overwriting the saved category.
        let current: Rc<RefCell<Option<Category>>> = Rc::default();
        {
            let sink = Rc::clone(&sink);
            let current = Rc::clone(&current);
            let rows_categories: Vec<Category> = rows
                .iter()
                .map(|(category, _, _)| category.clone())
                .collect();
            list.connect_row_selected(move |_, row| {
                let Some(category) = row.and_then(|row| rows_categories.get(row.index() as usize))
                else {
                    return;
                };
                if current
                    .borrow()
                    .as_ref()
                    .is_none_or(|current| current == category)
                {
                    return;
                }
                sink(ShellAction::Command(
                    warcraft_rustcorder::coordinator::Command::SetSelectedCategory {
                        category: category.clone(),
                    },
                ));
            });
        }

        let scrolled = gtk4::ScrolledWindow::new();
        scrolled.set_child(Some(&list));
        scrolled.set_vexpand(true);
        scrolled.set_propagate_natural_height(true);
        widget.append(&scrolled);

        let settings_icon = gtk4::Image::from_icon_name("emblem-system-symbolic");
        settings_icon.set_pixel_size(20);
        let settings_label = gtk4::Label::new(Some("Settings"));
        settings_label.set_xalign(0.0);
        settings_label.set_hexpand(true);
        let settings_warning = gtk4::Image::from_icon_name("dialog-warning-symbolic");
        settings_warning.set_visible(false);
        settings_warning.set_tooltip_text(Some("Settings need attention"));
        let settings_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        settings_box.set_margin_top(4);
        settings_box.set_margin_bottom(4);
        settings_box.set_margin_start(6);
        settings_box.set_margin_end(6);
        settings_box.append(&settings_icon);
        settings_box.append(&settings_label);
        settings_box.append(&settings_warning);
        let settings_button = gtk4::Button::new();
        settings_button.set_child(Some(&settings_box));
        settings_button.add_css_class("flat");
        settings_button.set_margin_start(6);
        settings_button.set_margin_end(6);
        settings_button.set_margin_bottom(6);
        settings_button.set_tooltip_text(Some("Settings"));
        {
            let sink = Rc::clone(&sink);
            settings_button.connect_clicked(move |_| {
                sink(ShellAction::OpenSettings);
            });
        }

        let update_button = icon_button("view-refresh-symbolic", "Check for updates");
        {
            let sink = Rc::clone(&sink);
            update_button.connect_clicked(move |_| {
                sink(ShellAction::CheckForUpdates);
            });
        }
        let test_button = icon_button("applications-science-symbolic", "Test recording");
        {
            let sink = Rc::clone(&sink);
            test_button.connect_clicked(move |_| {
                sink(ShellAction::TestRecording);
            });
        }
        let version = gtk4::Label::new(Some(concat!("Version ", env!("CARGO_PKG_VERSION"))));
        version.add_css_class("dim-label");
        version.add_css_class("caption");
        version.set_xalign(0.0);
        version.set_hexpand(true);
        let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        footer.set_margin_start(12);
        footer.set_margin_end(6);
        footer.set_margin_bottom(6);
        footer.append(&version);
        footer.append(&test_button);
        footer.append(&update_button);

        let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
        separator.set_margin_start(6);
        separator.set_margin_end(6);
        widget.append(&separator);
        widget.append(&settings_button);
        widget.append(&footer);

        Self {
            widget,
            status_card,
            list,
            rows,
            current,
            settings_warning,
        }
    }

    pub fn apply(&self, snapshot: &AppSnapshot) {
        let views = rows(snapshot);
        *self.current.borrow_mut() = Some(snapshot.config.interface.selected_category.clone());
        for (view, (_, row, count)) in views.iter().zip(&self.rows) {
            row.set_visible(view.visible);
            count.set_label(&view.count.to_string());
            if view.active {
                self.list.select_row(Some(row));
            }
        }
        self.settings_warning
            .set_visible(!snapshot.setup_problems.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use warcraft_rustcorder::config::Config;
    use warcraft_rustcorder::domain::RecorderStatus;

    fn snapshot(counts: Vec<(Category, usize)>, hide_empty: bool, manual: bool) -> AppSnapshot {
        let mut config = Config::default();
        config.interface.hide_empty_categories = hide_empty;
        config.manual.enabled = manual;
        let mut snapshot =
            crate::ui::window::tests::snapshot_with(RecorderStatus::Ready, config, Vec::new());
        snapshot.category_counts = counts;
        snapshot
    }

    #[test]
    fn hide_empty_hides_only_zero_categories_once_videos_exist() {
        let visible = |snapshot: &AppSnapshot, category: Category| {
            rows(snapshot)
                .into_iter()
                .find(|view| view.category == category)
                .expect("row exists")
                .visible
        };
        // An empty library hides nothing.
        let empty = snapshot(Vec::new(), true, false);
        assert!(rows(&empty).iter().all(|view| view.visible));

        let filled = snapshot(vec![(Category::Raids, 3)], true, false);
        assert!(visible(&filled, Category::Raids));
        assert!(!visible(&filled, Category::TwoVTwo));
        assert!(!visible(&filled, Category::Manual));

        // Manual stays while manual recording is enabled.
        let manual = snapshot(vec![(Category::Raids, 3)], true, true);
        assert!(visible(&manual, Category::Manual));
    }
}
