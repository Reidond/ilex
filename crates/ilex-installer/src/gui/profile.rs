//! The daily user page: a radio list (no daily profile, each public profile of
//! the image, a user name of the person's choice) and, for the last, two
//! fields (the name and an optional capacity). Drawn from `View::profile`;
//! nothing here decides what is offered, what is valid or what it says.

use super::app::{
    App, Msg, banner, continue_button, form, group, message_label, page, show_note, sync_banner,
};
use ilex_installer_flow::events::Event;
use ilex_installer_flow::flow::PageId;
use ilex_installer_flow::messages;
use ilex_installer_flow::view::{self, View};
use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;

pub struct ProfilePage {
    pub page: adw::NavigationPage,
    banner: adw::Banner,
    group: adw::PreferencesGroup,
    rows: Vec<(String, gtk::CheckButton, adw::ActionRow)>,
    note: gtk::Label,
    /// The custom user's name and capacity, shown while that row is chosen.
    custom: gtk::Box,
    name: adw::EntryRow,
    name_message: gtk::Label,
    size: adw::EntryRow,
    size_message: gtk::Label,
    pub button: gtk::Button,
}

impl ProfilePage {
    pub fn build(sender: &ComponentSender<App>) -> Self {
        let banner = banner(None);
        let button = continue_button(messages::CONTINUE_LABEL);
        let group = group(view::PROFILE_GROUP, "");
        let note = message_label();
        let name = adw::EntryRow::builder()
            .title(view::DAILY_NAME_LABEL)
            .name("daily-name")
            .build();
        name.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::LOWERCASE);
        let size = adw::EntryRow::builder()
            .title(view::DAILY_SIZE_LABEL)
            .name("daily-size")
            .build();
        size.set_input_hints(gtk::InputHints::NO_SPELLCHECK);
        size.set_input_purpose(gtk::InputPurpose::Digits);
        let name_message = message_label();
        let size_message = message_label();
        let fields = adw::PreferencesGroup::new();
        fields.add(&name);
        let size_group = adw::PreferencesGroup::new();
        size_group.add(&size);
        let custom = gtk::Box::new(gtk::Orientation::Vertical, 12);
        custom.append(&fields);
        custom.append(&name_message);
        custom.append(&size_group);
        custom.append(&size_message);
        custom.set_visible(false);
        let content = form(
            view::PROFILE_HEADING,
            view::PROFILE_INTRO,
            &[group.upcast_ref(), custom.upcast_ref(), note.upcast_ref()],
        );
        let page = page(
            view::PROFILE_TITLE,
            PageId::Profile.id(),
            Some(&banner),
            &content,
            Some(&button),
        );
        let input = sender.input_sender().clone();
        banner.connect_button_clicked({
            let input = input.clone();
            move |_| input.emit(Msg::Event(Event::Probe))
        });
        button.connect_clicked({
            let input = input.clone();
            move |_| input.emit(Msg::Event(Event::Continue))
        });
        name.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::DailyName(row.text().to_string())))
        });
        size.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::DailySize(row.text().to_string())))
        });
        // Enter in a field continues when the page is valid (the flow refuses
        // when it is not).
        for row in [&name, &size] {
            row.connect_entry_activated({
                let input = input.clone();
                move |_| input.emit(Msg::Event(Event::Continue))
            });
        }
        Self {
            page,
            banner,
            group,
            rows: Vec::new(),
            note,
            custom,
            name,
            name_message,
            size,
            size_message,
            button,
        }
    }

    pub fn sync(&mut self, v: &View, sender: &ComponentSender<App>) {
        let profile = &v.profile;
        sync_banner(&self.banner, &profile.chrome.banner);
        // One radio row per choice, rebuilt only when the list changes.
        let ids: Vec<&str> = profile.choices.iter().map(|c| c.id.as_str()).collect();
        if self
            .rows
            .iter()
            .map(|(id, _, _)| id.as_str())
            .collect::<Vec<_>>()
            != ids
        {
            for (_, _, row) in self.rows.drain(..) {
                self.group.remove(&row);
            }
            let mut first: Option<gtk::CheckButton> = None;
            for choice in &profile.choices {
                let check = gtk::CheckButton::new();
                if let Some(first) = &first {
                    check.set_group(Some(first));
                } else {
                    first = Some(check.clone());
                }
                let row = adw::ActionRow::builder()
                    .title(&choice.title)
                    .subtitle(&choice.subtitle)
                    .subtitle_lines(0)
                    .activatable_widget(&check)
                    .build();
                row.add_prefix(&check);
                let id = choice.id.clone();
                let input = sender.input_sender().clone();
                check.connect_toggled(move |check| {
                    if check.is_active() {
                        input.emit(Msg::Event(Event::Daily(id.clone())));
                    }
                });
                self.group.add(&row);
                self.rows.push((choice.id.clone(), check, row));
            }
        }
        for ((id, check, row), choice) in self.rows.iter().zip(&profile.choices) {
            if row.is_sensitive() != choice.enabled {
                if !choice.enabled {
                    release_focus(&self.page);
                }
                row.set_sensitive(choice.enabled);
            }
            if check.is_active() != (*id == profile.selected) {
                check.set_active(*id == profile.selected);
            }
        }
        show_note(&self.note, profile.note.as_ref(), &[]);
        match &profile.custom {
            None => self.custom.set_visible(false),
            Some(custom) => {
                self.custom.set_visible(true);
                // A field that has the focus gives it up first (GTK warns when
                // a focused text becomes insensitive).
                if self.name.is_sensitive() != profile.editable {
                    if !profile.editable {
                        release_focus(&self.page);
                    }
                    self.name.set_sensitive(profile.editable);
                    self.size.set_sensitive(profile.editable);
                }
                if self.name.text() != custom.name_value {
                    self.name.set_text(&custom.name_value);
                }
                if self.size.text() != custom.size_value {
                    self.size.set_text(&custom.size_value);
                }
                show_note(
                    &self.name_message,
                    custom.name_note.as_ref(),
                    &[self.name.upcast_ref()],
                );
                show_note(
                    &self.size_message,
                    Some(&custom.size_note),
                    &[self.size.upcast_ref()],
                );
            }
        }
        if let Some(next) = &profile.chrome.next {
            self.button.set_label(next.label);
            self.button.set_sensitive(next.enabled);
        }
    }
}

/// The focus leaves `page` when something on it has it.
fn release_focus(page: &adw::NavigationPage) {
    if let Some(root) = page.root()
        && let Some(focus) = root.focus()
        && focus.is_ancestor(page)
    {
        root.set_focus(None::<&gtk::Widget>);
    }
}
