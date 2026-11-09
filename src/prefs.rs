//! Preferences and Agent Configuration dialogs. Every control writes straight to
//! `settings::update`, which saves the file and restyles open windows.

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use std::rc::Rc;

use std::cell::{Cell, RefCell};

use crate::settings::{self, Agent, Appearance, Colors, CursorShape, PALETTE_NAMES};

pub fn show_preferences(parent: &impl IsA<gtk::Widget>) {
    let current = settings::get();
    let dialog = adw::PreferencesDialog::builder().title("Preferences").build();
    dialog.add(&appearance_page(&dialog, &current));
    dialog.add(&behavior_page(&current));
    dialog.present(Some(parent));
}

fn appearance_page(dialog: &adw::PreferencesDialog, current: &settings::Settings) -> adw::PreferencesPage {
    let appearance = &current.appearance;
    let page = adw::PreferencesPage::builder()
        .title("Appearance")
        .icon_name("applications-graphics-symbolic")
        .build();

    let controls = ThemeControls::new(dialog);
    controls.reload_list();
    controls.show_current();
    controls.connect();

    let theme_group = adw::PreferencesGroup::builder().title("Theme").build();
    theme_group.add(&controls.theme_row);
    let manage = adw::ActionRow::builder()
        .title("Your Themes")
        .subtitle("Save the current colors, font, and font size as a theme")
        .build();
    manage.add_suffix(&controls.delete_button);
    manage.add_suffix(&controls.save_button);
    theme_group.add(&manage);

    let buttons = &controls.colors;
    let custom = &controls.custom;
    custom.add_row(&suffix_row("Text", &buttons.foreground));
    custom.add_row(&suffix_row("Background", &buttons.background));
    custom.add_row(&suffix_row("Cursor", &buttons.cursor));
    custom.add_row(&suffix_row("Selection", &buttons.selection));
    custom.add_row(&suffix_row("Selected Text", &buttons.selection_text));
    custom.add_row(&palette_row("Normal Colors", &buttons.palette[..8], ""));
    custom.add_row(&palette_row("Bright Colors", &buttons.palette[8..], "Bright "));
    let reset = crate::compat::button_row(
        "Reset to Theme Colors",
        None,
        glib::clone!(
            #[strong]
            controls,
            move || {
                settings::update(|s| s.appearance.reset_custom_colors());
                controls.show_current();
            }
        ),
    );
    custom.add_row(&reset);
    theme_group.add(custom);
    page.add(&theme_group);

    let window = adw::PreferencesGroup::builder().title("Window").build();
    window.add(&opacity_row(appearance.background_opacity));
    page.add(&window);

    let text = adw::PreferencesGroup::builder().title("Text").build();
    text.add(&suffix_row("Font", &controls.font_button));
    text.add(&controls.font_size);
    page.add(&text);

    let cursor = adw::PreferencesGroup::builder().title("Cursor").build();
    let shapes: Vec<&str> = CursorShape::ALL.iter().map(|(_, name)| *name).collect();
    let shape_row = adw::ComboRow::builder()
        .title("Shape")
        .model(&gtk::StringList::new(&shapes))
        .selected(
            CursorShape::ALL.iter().position(|(shape, _)| *shape == appearance.cursor_shape).unwrap_or(0)
                as u32,
        )
        .build();
    shape_row.connect_selected_notify(|row| {
        let shape = CursorShape::ALL[row.selected() as usize].0;
        settings::update(|s| s.appearance.cursor_shape = shape);
    });
    cursor.add(&shape_row);
    let blink = adw::SwitchRow::builder().title("Blink").active(appearance.cursor_blink).build();
    blink.connect_active_notify(|row| {
        let blink = row.is_active();
        settings::update(|s| s.appearance.cursor_blink = blink);
    });
    cursor.add(&blink);
    page.add(&cursor);

    page
}

fn behavior_page(current: &settings::Settings) -> adw::PreferencesPage {
    let behavior = &current.behavior;
    let page = adw::PreferencesPage::builder()
        .title("Behavior")
        .icon_name("utilities-terminal-symbolic")
        .build();

    let terminal = adw::PreferencesGroup::builder().title("Terminal").build();
    let scrollback = adw::SpinRow::with_range(100.0, 1_000_000.0, 1000.0);
    scrollback.set_title("Scrollback Lines");
    scrollback.set_value(behavior.scrollback_lines as f64);
    scrollback.connect_value_notify(|row| {
        let lines = row.value() as u32;
        settings::update(|s| s.behavior.scrollback_lines = lines);
    });
    terminal.add(&scrollback);
    let bell = adw::SwitchRow::builder().title("Audible Bell").active(behavior.audible_bell).build();
    bell.connect_active_notify(|row| {
        let audible = row.is_active();
        settings::update(|s| s.behavior.audible_bell = audible);
    });
    terminal.add(&bell);
    page.add(&terminal);

    let tabs = adw::PreferencesGroup::builder().title("Tabs").build();
    let limit = adw::SpinRow::with_range(1.0, settings::MAX_TAB_SCROLL_LIMIT as f64, 1.0);
    limit.set_title("Tab Scroll Limit");
    limit.set_subtitle(&format!(
        "Tabs past this many scroll with the ‹ › buttons. Tabs share the bar's width, but never get \
         narrower than the bar divided by this number. Up to {}.",
        settings::MAX_TAB_SCROLL_LIMIT
    ));
    limit.set_value(behavior.tab_scroll_limit.clamp(1, settings::MAX_TAB_SCROLL_LIMIT) as f64);
    limit.connect_value_notify(|row| {
        let limit = row.value() as u32;
        settings::update(|s| s.behavior.tab_scroll_limit = limit);
    });
    tabs.add(&limit);
    page.add(&tabs);

    let shell = adw::PreferencesGroup::builder()
        .title("Shell")
        .description("Applies to new tabs. Leave empty to use your login shell ($SHELL).")
        .build();
    let shell_row = adw::EntryRow::builder()
        .title("Shell Command")
        .text(&behavior.shell)
        .show_apply_button(true)
        .build();
    shell_row.connect_apply(|row| {
        let shell = row.text().trim().to_owned();
        settings::update(|s| s.behavior.shell = shell);
    });
    shell.add(&shell_row);
    page.add(&shell);

    page
}

/// `restart` restarts the agent panel of the tab the dialog was opened from.
pub fn show_agent_config(parent: &impl IsA<gtk::Widget>, restart: impl Fn() + 'static) {
    let agent = settings::get().agent;
    let dialog = adw::PreferencesDialog::builder().title("Agent Configuration").build();
    let page = adw::PreferencesPage::new();

    let claude = adw::PreferencesGroup::builder()
        .title("Claude Code")
        .description("Path and model changes take effect when the agent restarts.")
        .build();
    let status = adw::ActionRow::builder().title("Status").subtitle("Checking…").build();
    claude.add(&status);
    refresh_status(&status);

    let path_row = adw::EntryRow::builder()
        .title("Claude Code Path (empty: search PATH)")
        .text(&agent.claude_path)
        .show_apply_button(true)
        .build();
    path_row.connect_apply(glib::clone!(
        #[weak]
        status,
        move |row| {
            let path = row.text().trim().to_owned();
            settings::update(|s| s.agent.claude_path = path);
            refresh_status(&status);
        }
    ));
    claude.add(&path_row);

    let model_names: Vec<&str> = Agent::MODELS.iter().map(|(_, name)| *name).collect();
    let model_row = adw::ComboRow::builder()
        .title("Model")
        .model(&gtk::StringList::new(&model_names))
        .selected(Agent::MODELS.iter().position(|(id, _)| *id == agent.model).unwrap_or(0) as u32)
        .build();
    model_row.connect_selected_notify(|row| {
        let model = Agent::MODELS[row.selected() as usize].0;
        settings::update(|s| s.agent.model = model.to_owned());
    });
    claude.add(&model_row);

    let auto_row = adw::SwitchRow::builder()
        .title("Start in Automatic Mode")
        .subtitle("New agent panels let Claude type without asking first")
        .active(agent.automatic_mode)
        .build();
    auto_row.connect_active_notify(|row| {
        let enabled = row.is_active();
        settings::update(|s| s.agent.automatic_mode = enabled);
    });
    claude.add(&auto_row);
    page.add(&claude);

    let instructions = adw::PreferencesGroup::builder().title("Instructions").build();
    let notes_button = gtk::Button::builder()
        .label("Open")
        .valign(gtk::Align::Center)
        .build();
    notes_button.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| open_personal_notes(&dialog)
    ));
    let notes_row = adw::ActionRow::builder()
        .title("Personal Instructions")
        .subtitle("Notes Claude reads at the start of every agent session")
        .build();
    notes_row.add_suffix(&notes_button);
    instructions.add(&notes_row);
    page.add(&instructions);

    let activity = adw::PreferencesGroup::builder().title("Activity").build();
    let log_button = gtk::Button::builder().label("Open").valign(gtk::Align::Center).build();
    log_button.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            let path = crate::safety::audit_log_path();
            if path.exists() {
                open_file(&dialog, &path);
            } else {
                dialog.add_toast(adw::Toast::new("No agent activity has been recorded yet"));
            }
        }
    ));
    let log_row = adw::ActionRow::builder()
        .title("Activity Log")
        .subtitle("Everything the agent typed, and whether it was automatic, approved, denied, or blocked")
        .build();
    log_row.add_suffix(&log_button);
    activity.add(&log_row);
    page.add(&activity);

    let privacy = adw::PreferencesGroup::builder()
        .title("Privacy")
        .description(
            "Claude reads your terminal only when it uses a Bishop tool. What it reads is sent to \
             Anthropic to generate responses.",
        )
        .build();
    let hide_secrets = adw::SwitchRow::builder()
        .title("Hide Secrets from Claude")
        .subtitle(
            "Private keys, API and cloud tokens, and passwords Bishop recognizes on screen are \
             replaced before Claude sees them, and in the activity log. Not every secret can be \
             recognized, so still avoid displaying them while the agent works.",
        )
        .active(!agent.share_secrets)
        .build();
    hide_secrets.connect_active_notify(|row| {
        let share = !row.is_active();
        settings::update(|s| s.agent.share_secrets = share);
    });
    privacy.add(&hide_secrets);
    page.add(&privacy);

    let actions = adw::PreferencesGroup::new();
    let restart_row =
        crate::compat::button_row("Restart Agent in This Tab", Some("view-refresh-symbolic"), restart);
    actions.add(&restart_row);
    page.add(&actions);

    dialog.add(&page);
    dialog.present(Some(parent));
}

fn refresh_status(row: &adw::ActionRow) {
    let Some(program) = settings::get().agent.claude_program() else {
        row.set_subtitle("Not found. Install Claude Code from claude.com/claude-code.");
        return;
    };
    let launched = gio::Subprocess::newv(
        &[program.as_os_str(), "--version".as_ref()],
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    );
    let process = match launched {
        Ok(process) => process,
        Err(e) => {
            row.set_subtitle(&format!("Could not run {}: {e}", program.display()));
            return;
        }
    };
    let row = row.downgrade();
    glib::spawn_future_local(async move {
        let output = process.communicate_utf8_future(None).await;
        let Some(row) = row.upgrade() else { return };
        match output {
            Ok((Some(version), _)) => {
                let version = version.trim();
                row.set_subtitle(&format!("{version} · {}", program.display()));
            }
            _ => row.set_subtitle(&format!("{} did not report a version", program.display())),
        }
    });
}

fn open_personal_notes(dialog: &adw::PreferencesDialog) {
    let path = crate::window::agent_dir().join("CLAUDE.local.md");
    if !path.exists() {
        let template = "# My notes for Bishop's agent\n\n\
                        Claude reads this file at the start of every agent session. Add anything \
                        it should know: hosts you manage, preferred tools, things to avoid.\n";
        if let Err(e) = crate::safety::private_dir(&crate::window::agent_dir())
            .and_then(|()| std::fs::write(&path, template))
        {
            dialog.add_toast(adw::Toast::new(&format!("Could not create {}: {e}", path.display())));
            return;
        }
    }
    open_file(dialog, &path);
}

fn open_file(dialog: &adw::PreferencesDialog, path: &std::path::Path) {
    let window = dialog.root().and_downcast::<gtk::Window>();
    gtk::FileLauncher::new(Some(&gio::File::for_path(path))).launch(
        window.as_ref(),
        None::<&gio::Cancellable>,
        glib::clone!(
            #[weak]
            dialog,
            move |result| {
                if let Err(e) = result {
                    dialog.add_toast(adw::Toast::new(&format!("Could not open the file: {e}")));
                }
            }
        ),
    );
}

fn monospace_font_dialog() -> gtk::FontDialog {
    let monospace = gtk::CustomFilter::new(|item| {
        item.downcast_ref::<pango::FontFamily>().is_some_and(|family| family.is_monospace())
            || item.downcast_ref::<pango::FontFace>().is_some_and(|face| face.family().is_monospace())
    });
    gtk::FontDialog::builder().title("Terminal Font").filter(&monospace).build()
}

/// The controls a theme sets: the theme list, custom colors, font, and font size. Picking a
/// theme loads all of them; saving a theme captures all of them.
struct ThemeControls {
    dialog: glib::WeakRef<adw::PreferencesDialog>,
    theme_row: adw::ComboRow,
    /// Theme ids, in list order.
    ids: RefCell<Vec<String>>,
    /// Set while the list is being rebuilt, so selection changes aren't treated as picks.
    reloading: Cell<bool>,
    save_button: gtk::Button,
    delete_button: gtk::Button,
    custom: adw::ExpanderRow,
    colors: Rc<ColorButtons>,
    font_button: gtk::FontDialogButton,
    font_size: adw::SpinRow,
}

impl ThemeControls {
    fn new(dialog: &adw::PreferencesDialog) -> Rc<Self> {
        let font_size = adw::SpinRow::with_range(6.0, 72.0, 1.0);
        font_size.set_title("Font Size");
        font_size.set_subtitle("Zoom adjusts each window from this size");
        Rc::new(Self {
            dialog: dialog.downgrade(),
            theme_row: adw::ComboRow::builder()
                .title("Theme")
                .subtitle("Sets the colors, font, and font size")
                .build(),
            ids: RefCell::new(Vec::new()),
            reloading: Cell::new(false),
            save_button: gtk::Button::builder().label("Save as Theme…").valign(gtk::Align::Center).build(),
            delete_button: gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Delete This Theme")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build(),
            custom: adw::ExpanderRow::builder()
                .title("Custom Colors")
                .subtitle("Override every color the theme sets, including the 16 colors programs use")
                .show_enable_switch(true)
                .build(),
            colors: Rc::new(ColorButtons::new()),
            font_button: gtk::FontDialogButton::builder()
                .dialog(&monospace_font_dialog())
                .level(gtk::FontLevel::Family)
                .use_font(true)
                .valign(gtk::Align::Center)
                .build(),
            font_size,
        })
    }

    /// Rebuilds the theme list (built-in, then saved) and selects the current theme.
    fn reload_list(&self) {
        let themes = settings::themes();
        let names: Vec<String> = themes
            .iter()
            .map(|t| if t.builtin { t.name.clone() } else { format!("{} (yours)", t.name) })
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let current = settings::get().appearance.theme;
        self.reloading.set(true);
        *self.ids.borrow_mut() = themes.iter().map(|t| t.id.clone()).collect();
        self.theme_row.set_model(Some(&gtk::StringList::new(&names)));
        self.theme_row.set_selected(themes.iter().position(|t| t.id == current).unwrap_or(0) as u32);
        self.reloading.set(false);
    }

    /// Shows the current settings in every control.
    fn show_current(&self) {
        let appearance = settings::get().appearance;
        self.custom.set_enable_expansion(appearance.custom_colors);
        self.colors.show(&appearance.colors());
        self.font_button.set_font_desc(&pango::FontDescription::from_string(&appearance.font_family));
        self.font_size.set_value(appearance.font_size as f64);
        self.delete_button.set_sensitive(!settings::theme(&appearance.theme).builtin);
    }

    fn connect(self: &Rc<Self>) {
        self.colors.connect();
        let this = Rc::downgrade(self);
        self.theme_row.connect_selected_notify(move |row| {
            let Some(this) = this.upgrade() else { return };
            if this.reloading.get() {
                return;
            }
            let Some(id) = this.ids.borrow().get(row.selected() as usize).cloned() else { return };
            let theme = settings::theme(&id);
            settings::update(|s| s.appearance.use_theme(&theme));
            this.show_current();
        });

        let this = Rc::downgrade(self);
        self.custom.connect_enable_expansion_notify(move |row| {
            let Some(this) = this.upgrade() else { return };
            let enabled = row.enables_expansion();
            settings::update(|s| {
                s.appearance.custom_colors = enabled;
                // Start from the current theme the first time.
                if enabled && !s.appearance.has_custom_colors() {
                    s.appearance.reset_custom_colors();
                }
            });
            this.colors.show(&settings::get().appearance.colors());
            if enabled {
                row.set_expanded(true);
            }
        });

        self.font_button.connect_font_desc_notify(|button| {
            if let Some(family) = button.font_desc().and_then(|desc| desc.family()) {
                settings::update(|s| s.appearance.font_family = family.to_string());
            }
        });
        self.font_size.connect_value_notify(|row| {
            let size = row.value() as u32;
            settings::update(|s| s.appearance.font_size = size);
        });

        let this = Rc::downgrade(self);
        self.save_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.ask_save();
            }
        });
        let this = Rc::downgrade(self);
        self.delete_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.ask_delete();
            }
        });
    }

    fn toast(&self, message: &str) {
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.add_toast(adw::Toast::new(message));
        }
    }

    fn ask_save(self: &Rc<Self>) {
        let Some(parent) = self.dialog.upgrade() else { return };
        let current = settings::theme(&settings::get().appearance.theme);
        let entry = gtk::Entry::builder()
            .placeholder_text("Theme name")
            .text(if current.builtin { "" } else { current.name.as_str() })
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::builder()
            .heading("Save Theme")
            .body("Saves the current colors, font, and font size. Saving with the name of one of your themes updates it.")
            .extra_child(&entry)
            .default_response("save")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("save", "Save");
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        let this = Rc::downgrade(self);
        let name_entry = entry.clone();
        dialog.connect_response(Some("save"), move |_, _| {
            let Some(this) = this.upgrade() else { return };
            let name = name_entry.text().trim().to_owned();
            let existed = settings::themes().iter().any(|t| !t.builtin && t.name.eq_ignore_ascii_case(&name));
            match settings::save_theme(&name, &settings::get().appearance) {
                Ok(theme) => {
                    settings::update(|s| s.appearance.use_theme(&theme));
                    this.reload_list();
                    this.show_current();
                    this.toast(&format!("{} “{}”", if existed { "Updated" } else { "Saved" }, theme.name));
                }
                Err(e) => this.toast(&e),
            }
        });
        dialog.present(Some(&parent));
        entry.grab_focus();
    }

    fn ask_delete(self: &Rc<Self>) {
        let Some(parent) = self.dialog.upgrade() else { return };
        let theme = settings::theme(&settings::get().appearance.theme);
        if theme.builtin {
            return;
        }
        let dialog = adw::AlertDialog::builder()
            .heading(format!("Delete “{}”?", theme.name))
            .body("The theme is removed for good. Bishop switches back to the default theme.")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        let this = Rc::downgrade(self);
        dialog.connect_response(Some("delete"), move |_, _| {
            let Some(this) = this.upgrade() else { return };
            match settings::delete_theme(&theme.id) {
                Ok(()) => {
                    let default = settings::theme("");
                    settings::update(|s| s.appearance.use_theme(&default));
                    this.reload_list();
                    this.show_current();
                    this.toast(&format!("Deleted “{}”", theme.name));
                }
                Err(e) => this.toast(&e),
            }
        });
        dialog.present(Some(&parent));
    }
}

/// Writes a picked color into its field.
type StoreColor = dyn Fn(&mut Appearance, String);

/// One color button per configurable terminal color.
struct ColorButtons {
    foreground: gtk::ColorDialogButton,
    background: gtk::ColorDialogButton,
    cursor: gtk::ColorDialogButton,
    selection: gtk::ColorDialogButton,
    selection_text: gtk::ColorDialogButton,
    palette: Vec<gtk::ColorDialogButton>,
    /// Set while `show` updates the buttons, so that isn't saved as a user edit.
    showing: std::cell::Cell<bool>,
}

impl ColorButtons {
    fn new() -> Self {
        Self {
            foreground: color_button(),
            background: color_button(),
            cursor: color_button(),
            selection: color_button(),
            selection_text: color_button(),
            palette: (0..16).map(|_| color_button()).collect(),
            showing: std::cell::Cell::new(false),
        }
    }

    fn show(&self, colors: &Colors) {
        self.showing.set(true);
        self.foreground.set_rgba(&rgba(&colors.foreground));
        self.background.set_rgba(&rgba(&colors.background));
        self.cursor.set_rgba(&rgba(&colors.cursor));
        self.selection.set_rgba(&rgba(&colors.selection));
        self.selection_text.set_rgba(&rgba(&colors.selection_text));
        for (button, color) in self.palette.iter().zip(&colors.palette) {
            button.set_rgba(&rgba(color));
        }
        self.showing.set(false);
    }

    /// Saves each button's color when the user picks one.
    fn connect(self: &Rc<Self>) {
        let bind = |button: &gtk::ColorDialogButton, store: Box<StoreColor>| {
            let buttons = Rc::downgrade(self);
            button.connect_rgba_notify(move |button| {
                let Some(buttons) = buttons.upgrade() else { return };
                if buttons.showing.get() {
                    return;
                }
                let color = hex(&button.rgba());
                settings::update(|s| {
                    if !s.appearance.has_custom_colors() {
                        s.appearance.reset_custom_colors();
                    }
                    s.appearance.custom_colors = true;
                    store(&mut s.appearance, color);
                });
            });
        };
        bind(&self.foreground, Box::new(|a, c| a.foreground = c));
        bind(&self.background, Box::new(|a, c| a.background = c));
        bind(&self.cursor, Box::new(|a, c| a.cursor = c));
        bind(&self.selection, Box::new(|a, c| a.selection = c));
        bind(&self.selection_text, Box::new(|a, c| a.selection_text = c));
        for (index, button) in self.palette.iter().enumerate() {
            bind(button, Box::new(move |a, c| a.palette[index] = c));
        }
    }
}

/// A slider and a number box sharing one value, so dragging updates the number and typing
/// a number moves the slider.
fn opacity_row(opacity: u32) -> adw::ActionRow {
    let adjustment = gtk::Adjustment::new(opacity.min(100) as f64, 0.0, 100.0, 1.0, 10.0, 0.0);
    let slider = gtk::Scale::builder()
        .adjustment(&adjustment)
        .draw_value(false)
        .width_request(180)
        .valign(gtk::Align::Center)
        .build();
    let number = gtk::SpinButton::builder()
        .adjustment(&adjustment)
        .digits(0)
        .numeric(true)
        .valign(gtk::Align::Center)
        .tooltip_text("Opacity in percent")
        .build();
    adjustment.connect_value_changed(|adjustment| {
        let opacity = adjustment.value().round() as u32;
        settings::update(|s| s.appearance.background_opacity = opacity);
    });

    let row = adw::ActionRow::builder()
        .title("Background Opacity")
        .subtitle("Applies to the terminal and agent panel; text stays solid")
        .build();
    row.add_suffix(&slider);
    row.add_suffix(&number);
    row.add_suffix(&gtk::Label::new(Some("%")));
    row
}

fn palette_row(title: &str, buttons: &[gtk::ColorDialogButton], prefix: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let swatches = gtk::Box::builder().spacing(2).valign(gtk::Align::Center).build();
    for (button, name) in buttons.iter().zip(PALETTE_NAMES) {
        button.set_tooltip_text(Some(&format!("{prefix}{name}")));
        swatches.append(button);
    }
    row.add_suffix(&swatches);
    row
}

fn color_button() -> gtk::ColorDialogButton {
    let button = gtk::ColorDialogButton::new(Some(gtk::ColorDialog::builder().with_alpha(false).build()));
    button.set_valign(gtk::Align::Center);
    button
}

fn suffix_row(title: &str, widget: &impl IsA<gtk::Widget>) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    row.add_suffix(widget);
    row
}

pub fn rgba(color: &str) -> gdk::RGBA {
    gdk::RGBA::parse(color).unwrap_or(gdk::RGBA::BLACK)
}

fn hex(color: &gdk::RGBA) -> String {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", channel(color.red()), channel(color.green()), channel(color.blue()))
}
