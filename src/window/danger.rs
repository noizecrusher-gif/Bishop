//! Danger Mode: a tab where Bishop's guardrails are off. Nothing the agent sends is held for
//! approval as destructive, and it may type at password prompts. With automatic mode also on,
//! the agent acts entirely on its own.
//!
//! Turning it on takes the user's login password the first time after Bishop starts (see
//! `auth`). It is never saved: every tab, and every start of Bishop, begins with it off.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::Session;
use crate::auth::{self, Outcome};

const WARNING: &str = "You are about to enable Danger Mode, doing so will allow your AI agent to \
execute dangerous commands without restrictions. Enable at your own risk";

impl Session {
    pub(super) fn danger_switch_changed(self: &Rc<Self>, active: bool) {
        if active == self.danger_mode.get() {
            return;
        }
        if !active {
            self.set_danger_mode(false);
            return;
        }
        if auth::danger_unlocked() {
            self.set_danger_mode(true);
            return;
        }
        // Not yet: the switch turns on once the password has been checked.
        self.danger_switch.set_active(false);
        self.ask_for_password();
    }

    fn set_danger_mode(&self, on: bool) {
        self.danger_mode.set(on);
        self.danger_banner.set_revealed(on);
        self.tab.set_danger(on);
        if on {
            self.danger_row.add_css_class("on");
            self.audit("danger_mode", "", "turned on by user", None);
            self.toast("Danger Mode on: Bishop no longer holds risky input for approval in this tab");
        } else {
            self.danger_row.remove_css_class("on");
            self.audit("danger_mode", "", "turned off by user", None);
            self.toast("Danger Mode off: Bishop's safety checks are back on");
        }
    }

    fn ask_for_password(self: &Rc<Self>) {
        let heading = gtk::Label::builder().label("Enable Danger Mode?").css_classes(["title-2"]).wrap(true).build();
        let message = gtk::Label::builder()
            .label(WARNING)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(44)
            .build();
        let entry = gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .placeholder_text(format!("Password for {}", glib::user_name().to_string_lossy()))
            .activates_default(true)
            .build();
        let error = gtk::Label::builder().css_classes(["error"]).wrap(true).visible(false).build();
        let cancel = gtk::Button::builder().label("Cancel").hexpand(true).build();
        let enable = gtk::Button::builder()
            .label("Enable Danger Mode")
            .hexpand(true)
            .sensitive(false)
            .css_classes(["destructive-action"])
            .build();
        let buttons = gtk::Box::builder().spacing(12).homogeneous(true).margin_top(6).build();
        buttons.append(&cancel);
        buttons.append(&enable);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        body.append(
            &gtk::Image::builder()
                .icon_name("dialog-warning-symbolic")
                .pixel_size(48)
                .css_classes(["error"])
                .build(),
        );
        body.append(&heading);
        body.append(&message);
        body.append(&entry);
        body.append(&error);
        body.append(&buttons);

        let dialog = adw::Dialog::builder()
            .title("Danger Mode")
            .content_width(420)
            .child(&body)
            .default_widget(&enable)
            .focus_widget(&entry)
            .build();

        entry.connect_changed(glib::clone!(
            #[weak]
            enable,
            move |entry| enable.set_sensitive(!entry.text().is_empty())
        ));
        cancel.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            move |_| {
                dialog.close();
            }
        ));
        let weak = Rc::downgrade(self);
        enable.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            #[weak]
            entry,
            #[weak]
            error,
            #[weak]
            cancel,
            move |enable| {
                let password = entry.text().to_string();
                entry.set_text("");
                if password.is_empty() {
                    return;
                }
                for widget in [enable.upcast_ref::<gtk::Widget>(), entry.upcast_ref(), cancel.upcast_ref()] {
                    widget.set_sensitive(false);
                }
                enable.set_label("Checking…");
                error.set_visible(false);
                let (tx, rx) = async_channel::bounded(1);
                std::thread::spawn(move || {
                    let _ = tx.send_blocking(auth::check_password(password));
                });
                let weak = weak.clone();
                let enable = enable.clone();
                glib::spawn_future_local(async move {
                    let outcome = rx.recv().await.unwrap_or(Outcome::Unavailable("the check stopped".into()));
                    let Some(state) = weak.upgrade() else {
                        dialog.close();
                        return;
                    };
                    match outcome {
                        Outcome::Correct => {
                            auth::unlock_danger();
                            dialog.close();
                            state.danger_switch.set_active(true);
                        }
                        Outcome::Wrong | Outcome::Unavailable(_) => {
                            let text = match &outcome {
                                Outcome::Unavailable(why) => {
                                    format!("Bishop couldn't check your password: {why}.")
                                }
                                _ => "That password is incorrect. Try again.".to_owned(),
                            };
                            state.audit("danger_mode", "", "password not accepted", None);
                            error.set_text(&text);
                            error.set_visible(true);
                            enable.set_label("Enable Danger Mode");
                            entry.set_sensitive(true);
                            cancel.set_sensitive(true);
                            entry.grab_focus();
                        }
                    }
                });
            }
        ));
        dialog.present(Some(&self.window));
    }
}
