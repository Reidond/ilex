//! The network page, drawn from `View::network`: the wired adapter, what is
//! wrong (or that the registry can be reached), the Wi-Fi networks in range
//! with a password field for the chosen one, Connect and Refresh. Nothing here
//! decides anything; the flow does (`ilex-installer-flow`'s network module).

use super::app::{
    App, Msg, banner, continue_button, form, group, message_label, page, property_row, secret_of,
    show_note, sync_banner,
};
use ilex_installer_flow::events::Event;
use ilex_installer_flow::flow::PageId;
use ilex_installer_flow::messages;
use ilex_installer_flow::view::{self, View};
use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;

pub struct NetworkPage {
    pub page: adw::NavigationPage,
    banner: adw::Banner,
    wired: adw::ActionRow,
    status: gtk::Label,
    group: adw::PreferencesGroup,
    empty: gtk::Label,
    rows: Vec<(String, gtk::CheckButton, adw::ActionRow)>,
    shown: Vec<(String, String, bool)>,
    password_group: adw::PreferencesGroup,
    password: adw::PasswordEntryRow,
    password_note: gtk::Label,
    busy: gtk::Box,
    busy_label: gtk::Label,
    connect: gtk::Button,
    refresh: gtk::Button,
    pub button: gtk::Button,
    /// Whether Continue could be pressed at the last sync: when it can be, it
    /// has the focus (the person plugged a cable in, or joined a network, and
    /// presses Enter).
    can_continue: bool,
    /// The flow's wipe count the password field was last cleared at.
    wiped: u64,
}

impl NetworkPage {
    pub fn build(sender: &ComponentSender<App>, wiped: u64) -> Self {
        let banner = banner(None);
        let button = continue_button(messages::CONTINUE_LABEL);
        let wired = property_row(view::NETWORK_WIRED);
        let wired_group = group("", "");
        wired_group.add(&wired);
        let status = message_label();
        let wifi = group(view::NETWORK_WIFI, "");
        let refresh = gtk::Button::with_label(view::NETWORK_REFRESH);
        refresh.add_css_class("flat");
        refresh.set_valign(gtk::Align::Center);
        wifi.set_header_suffix(Some(&refresh));
        let empty = message_label();
        let password = adw::PasswordEntryRow::builder()
            .title(view::NETWORK_PASSWORD)
            .name("wifi-password")
            .build();
        // As the other password fields: nothing typed here is kept for Ctrl+Z.
        password.set_enable_undo(false);
        let password_group = group("", "");
        password_group.add(&password);
        let password_note = message_label();
        let connect = gtk::Button::with_label(view::NETWORK_CONNECT);
        connect.add_css_class("pill");
        connect.add_css_class("suggested-action");
        connect.set_halign(gtk::Align::Start);
        let busy_label = gtk::Label::new(None);
        let busy = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        busy.append(&adw::Spinner::new());
        busy.append(&busy_label);
        busy.set_visible(false);
        let content = form(
            view::NETWORK_HEADING,
            view::NETWORK_INTRO,
            &[
                wired_group.upcast_ref(),
                status.upcast_ref(),
                wifi.upcast_ref(),
                empty.upcast_ref(),
                password_group.upcast_ref(),
                password_note.upcast_ref(),
                connect.upcast_ref(),
                busy.upcast_ref(),
            ],
        );
        let page = page(
            view::NETWORK_TITLE,
            PageId::Network.id(),
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
        connect.connect_clicked({
            let input = input.clone();
            move |_| input.emit(Msg::Event(Event::Connect))
        });
        refresh.connect_clicked({
            let input = input.clone();
            move |_| input.emit(Msg::Event(Event::Refresh))
        });
        password.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::NetworkPassword(secret_of(row))))
        });
        password.connect_entry_activated({
            let input = input.clone();
            move |_| input.emit(Msg::Event(Event::Connect))
        });
        page.connect_shown({
            let button = button.clone();
            move |_| {
                button.grab_focus();
            }
        });
        Self {
            page,
            banner,
            wired,
            status,
            group: wifi,
            empty,
            rows: Vec::new(),
            shown: Vec::new(),
            password_group,
            password,
            password_note,
            busy,
            busy_label,
            connect,
            refresh,
            button,
            can_continue: false,
            wiped,
        }
    }

    pub fn sync(&mut self, v: &View, sender: &ComponentSender<App>) {
        let network = &v.network;
        sync_banner(&self.banner, &network.chrome.banner);
        if let Some(next) = &network.chrome.next {
            self.button.set_label(next.label);
            self.button.set_sensitive(next.enabled);
            if next.enabled && !self.can_continue && self.page.is_mapped() {
                self.button.grab_focus();
            }
            self.can_continue = next.enabled;
        }
        self.page.set_can_pop(network.chrome.can_pop);
        self.wired.set_subtitle(&network.wired);
        show_note(&self.status, Some(&network.status), &[]);

        // The list, rebuilt only when it changed.
        let key: Vec<(String, String, bool)> = network
            .networks
            .iter()
            .map(|n| (n.id.clone(), n.subtitle.clone(), n.available))
            .collect();
        if key != self.shown {
            for (_, _, row) in self.rows.drain(..) {
                self.group.remove(&row);
            }
            let mut first: Option<gtk::CheckButton> = None;
            for network in &network.networks {
                let row = adw::ActionRow::builder()
                    .title(glib_markup_escape(&network.id))
                    .subtitle(&network.subtitle)
                    .subtitle_lines(0)
                    .build();
                let check = gtk::CheckButton::new();
                if let Some(first) = &first {
                    check.set_group(Some(first));
                } else {
                    first = Some(check.clone());
                }
                row.add_prefix(&check);
                row.set_activatable_widget(Some(&check));
                row.add_suffix(&gtk::Image::from_icon_name(signal_icon(network)));
                row.set_sensitive(network.available);
                let id = network.id.clone();
                let input = sender.input_sender().clone();
                check.connect_toggled(move |check| {
                    if check.is_active() {
                        input.emit(Msg::Event(Event::Network(id.clone())));
                    }
                });
                self.group.add(&row);
                self.rows.push((network.id.clone(), check, row));
            }
            self.shown = key;
        }
        for (id, check, _) in &self.rows {
            if check.is_active() != (*id == network.selected) {
                check.set_active(*id == network.selected);
            }
        }
        self.group.set_visible(!network.networks.is_empty());
        self.group.set_sensitive(network.editable);
        match &network.empty {
            Some(text) => {
                self.empty.set_css_classes(&["caption", "dim-label"]);
                self.empty.set_label(text);
                self.empty.set_visible(true);
            }
            None => self.empty.set_visible(false),
        }

        // The password of the chosen network, cleared when the flow wiped it.
        if network.wiped != self.wiped {
            self.password.set_text("");
            self.wiped = network.wiped;
        }
        self.password_group.set_visible(network.password.is_some());
        self.password_group.set_sensitive(network.editable);
        if let Some(field) = &network.password {
            self.password.set_title(field.label);
        }
        show_note(&self.password_note, network.password_note.as_ref(), &[]);
        self.connect.set_label(&network.connect.label);
        self.connect.set_visible(!network.networks.is_empty());
        self.connect.set_sensitive(network.connect.enabled);
        self.refresh.set_visible(network.refresh.visible);
        self.refresh.set_sensitive(network.refresh.enabled);
        self.busy.set_visible(network.busy.is_some());
        if let Some(text) = &network.busy {
            self.busy_label.set_label(text);
        }
    }
}

/// A network's name is shown as it is: markup in it (`&`, `<`) is not markup.
fn glib_markup_escape(text: &str) -> String {
    relm4::gtk::glib::markup_escape_text(text).to_string()
}

/// The icon that says how strong a network's signal is.
fn signal_icon(network: &view::WifiRow) -> &'static str {
    match network.signal {
        70.. => "network-wireless-signal-excellent-symbolic",
        40..=69 => "network-wireless-signal-good-symbolic",
        _ => "network-wireless-signal-weak-symbolic",
    }
}
