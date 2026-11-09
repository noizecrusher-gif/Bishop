//! Widgets from newer libadwaita releases, used when the running system has them.
//!
//! Bishop is built against the libraries in Ubuntu 24.04 (GTK 4.14, libadwaita 1.5, VTE 0.76)
//! so one binary runs on every supported distro. Newer widgets are created by type name at
//! run time, with an older equivalent as the fallback.

use adw::prelude::*;
use gtk::glib;

/// Creates the widget registered as `type_name`, if this libadwaita has it.
fn newer_widget(type_name: &str) -> Option<gtk::Widget> {
    let type_ = glib::Type::from_name(type_name)?;
    glib::Object::with_type(type_).downcast::<gtk::Widget>().ok()
}

/// A small busy indicator: `AdwSpinner` (libadwaita 1.6+), else `GtkSpinner`.
pub fn spinner() -> gtk::Widget {
    newer_widget("AdwSpinner").unwrap_or_else(|| gtk::Spinner::builder().spinning(true).build().upcast())
}

/// A row that acts as a button in a preferences list: `AdwButtonRow` (libadwaita 1.6+), else
/// an activatable `AdwActionRow`.
pub fn button_row(title: &str, icon: Option<&str>, on_activate: impl Fn() + 'static) -> gtk::Widget {
    if let Some(row) = newer_widget("AdwButtonRow") {
        row.set_property("title", title);
        if let Some(icon) = icon {
            row.set_property("start-icon-name", icon);
        }
        row.connect_local("activated", false, move |_| {
            on_activate();
            None
        });
        return row;
    }
    let row = adw::ActionRow::builder().title(title).activatable(true).build();
    if let Some(icon) = icon {
        row.add_prefix(&gtk::Image::from_icon_name(icon));
    }
    row.connect_activated(move |_| on_activate());
    row.upcast()
}
