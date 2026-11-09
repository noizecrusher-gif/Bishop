//! The tab bar: an always-present New Tab button, then the tabs, added left to right. With
//! only one tab open, the tab itself is hidden.
//!
//! Tabs share the bar's width equally. The tab scroll limit (Preferences > Behavior) sets
//! how many fit before the bar scrolls with its ‹ › buttons, which is also the narrowest a
//! tab gets: a tab is never narrower than the bar's width divided by the limit.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::settings::MAX_TAB_SCROLL_LIMIT;

pub struct TabBar {
    pub widget: gtk::Box,
    pub new_button: gtk::Button,
    prev: gtk::Button,
    next: gtk::Button,
    scroller: gtk::ScrolledWindow,
    /// Homogeneous, so every tab gets the same width.
    strip: gtk::Box,
    limit: Cell<u32>,
    /// A tab to scroll into view once the strip has been laid out.
    reveal: RefCell<Option<gtk::Widget>>,
    layout_queued: Cell<bool>,
}

impl TabBar {
    pub fn new(limit: u32) -> Rc<Self> {
        let new_button = gtk::Button::builder()
            .icon_name("tab-new-symbolic")
            .tooltip_text("New Tab (Ctrl+Shift+T)")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        let arrow = |icon: &str, tooltip: &str| {
            gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .visible(false)
                .build()
        };
        let prev = arrow("go-previous-symbolic", "Scroll Tabs Left");
        let next = arrow("go-next-symbolic", "Scroll Tabs Right");
        let strip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).homogeneous(true).build();
        let viewport = gtk::Viewport::builder().child(&strip).build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&viewport)
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .css_classes(["bishop-tabbar"])
            .build();
        widget.append(&new_button);
        widget.append(&prev);
        widget.append(&scroller);
        widget.append(&next);

        let bar = Rc::new(Self {
            widget,
            new_button,
            prev: prev.clone(),
            next: next.clone(),
            scroller: scroller.clone(),
            strip,
            limit: Cell::new(limit),
            reveal: RefCell::new(None),
            layout_queued: Cell::new(false),
        });

        prev.connect_clicked(glib::clone!(
            #[weak]
            bar,
            move |_| bar.scroll_by(-1.0)
        ));
        next.connect_clicked(glib::clone!(
            #[weak]
            bar,
            move |_| bar.scroll_by(1.0)
        ));
        let adjustment = scroller.hadjustment();
        // The bar was resized: tabs get a new minimum width.
        adjustment.connect_page_size_notify(glib::clone!(
            #[weak]
            bar,
            move |_| bar.queue_layout()
        ));
        adjustment.connect_changed(glib::clone!(
            #[weak]
            bar,
            move |_| {
                bar.try_reveal();
                bar.update_arrows();
            }
        ));
        adjustment.connect_value_changed(glib::clone!(
            #[weak]
            bar,
            move |_| bar.update_arrows()
        ));
        // A mouse wheel scrolls the tabs one at a time.
        let wheel = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        wheel.connect_scroll(glib::clone!(
            #[weak]
            bar,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, _, dy| {
                bar.scroll_by(dy.signum());
                glib::Propagation::Stop
            }
        ));
        scroller.add_controller(wheel);
        bar
    }

    /// Adds `tab` at the right end.
    pub fn add(self: &Rc<Self>, tab: &Tab) {
        self.strip.append(&tab.root);
        self.queue_layout();
    }

    pub fn remove(self: &Rc<Self>, tab: &Tab) {
        self.strip.remove(&tab.root);
        self.queue_layout();
    }

    pub fn set_limit(self: &Rc<Self>, limit: u32) {
        if self.limit.replace(limit) != limit {
            self.queue_layout();
        }
    }

    /// Scrolls so `tab` is fully visible, now or once the bar has been laid out.
    pub fn reveal(&self, tab: &Tab) {
        self.reveal.replace(Some(tab.root.clone().upcast()));
        self.try_reveal();
    }

    /// Sizes are read from the last allocation, so recompute them after the current one.
    fn queue_layout(self: &Rc<Self>) {
        if self.layout_queued.replace(true) {
            return;
        }
        let bar = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(bar) = bar.upgrade() {
                bar.layout_queued.set(false);
                bar.layout();
            }
        });
    }

    fn layout(&self) {
        let count = self.tabs().len();
        let limit = self.limit.get().clamp(1, MAX_TAB_SCROLL_LIMIT) as usize;
        let scrolling = count > limit && count > 1;
        // A lone tab isn't shown: tabs appear once there are two.
        for tab in self.tabs() {
            tab.set_visible(count > 1);
        }
        self.prev.set_visible(scrolling);
        self.next.set_visible(scrolling);
        let width = self.scroller.width();
        if width > 0 {
            let narrowest = (width / limit as i32).max(1);
            for tab in self.tabs() {
                if tab.width_request() != narrowest {
                    tab.set_width_request(narrowest);
                }
            }
        }
        self.try_reveal();
        self.update_arrows();
    }

    fn tabs(&self) -> Vec<gtk::Widget> {
        std::iter::successors(self.strip.first_child(), |w| w.next_sibling()).collect()
    }

    /// The width each tab gets once laid out: an equal share of the bar, but no less than
    /// the bar divided by the limit. Computed rather than measured, so it's right even
    /// before tabs that were just added have been allocated.
    fn tab_width(&self) -> f64 {
        let count = self.tabs().len().max(1) as f64;
        let limit = self.limit.get().clamp(1, MAX_TAB_SCROLL_LIMIT) as f64;
        let page = self.scroller.hadjustment().page_size();
        if count > limit { (page / limit).floor() } else { page / count }
    }

    /// Scrolls by whole tabs, landing on a tab's edge.
    fn scroll_by(&self, tabs: f64) {
        let adjustment = self.scroller.hadjustment();
        let width = self.tab_width();
        if width <= 0.0 {
            return;
        }
        self.reveal.take();
        let target = ((adjustment.value() / width).round() + tabs) * width;
        adjustment.set_value(target.clamp(adjustment.lower(), adjustment.upper() - adjustment.page_size()));
    }

    fn try_reveal(&self) {
        let Some(widget) = self.reveal.borrow().clone() else { return };
        let Some(index) = self.tabs().iter().position(|tab| *tab == widget) else {
            self.reveal.take();
            return;
        };
        let width = self.tab_width();
        let adjustment = self.scroller.hadjustment();
        let page = adjustment.page_size();
        if width <= 0.0 || page <= 0.0 {
            return;
        }
        // Until the strip has its new width, the scroll range is too short to get there.
        let laid_out = adjustment.upper() >= width * self.tabs().len() as f64 - 1.0;
        let (start, end) = (index as f64 * width, (index + 1) as f64 * width);
        let mut value = adjustment.value();
        if start < value {
            value = start;
        } else if end > value + page {
            value = end - page;
        }
        adjustment.set_value(value);
        let value = adjustment.value();
        if laid_out && value <= start + 0.5 && value + page >= end - 0.5 {
            self.reveal.take();
        }
    }

    fn update_arrows(&self) {
        let adjustment = self.scroller.hadjustment();
        let value = adjustment.value();
        self.prev.set_sensitive(value > adjustment.lower() + 0.5);
        self.next.set_sensitive(value < adjustment.upper() - adjustment.page_size() - 0.5);
    }
}

/// What a tab's agent is doing, shown as an icon so background tabs aren't forgotten.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Activity {
    Idle,
    Busy,
    /// Waiting for the user to approve input.
    Waiting,
    Stopped,
}

/// One tab in the bar.
#[derive(Clone)]
pub struct Tab {
    pub root: gtk::Box,
    pub close: gtk::Button,
    label: gtk::Label,
    spinner: gtk::Widget,
    icon: gtk::Image,
}

impl Tab {
    pub fn new() -> Self {
        let label = gtk::Label::builder()
            .label("Terminal")
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let spinner = crate::compat::spinner();
        spinner.set_visible(false);
        spinner.set_tooltip_text(Some("Claude is working"));
        let icon = gtk::Image::builder().visible(false).build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close Tab (Ctrl+Shift+W)")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["bishop-tab"])
            .build();
        root.append(&spinner);
        root.append(&icon);
        root.append(&label);
        root.append(&close);
        Self { root, close, label, spinner, icon }
    }

    pub fn set_title(&self, title: &str) {
        let title = if title.is_empty() { "Terminal" } else { title };
        self.label.set_text(title);
        self.root.set_tooltip_text(Some(title));
    }

    pub fn set_selected(&self, selected: bool) {
        if selected {
            self.root.add_css_class("selected");
        } else {
            self.root.remove_css_class("selected");
        }
    }

    pub fn set_activity(&self, activity: Activity) {
        self.spinner.set_visible(activity == Activity::Busy);
        let (icon, class, tooltip) = match activity {
            Activity::Waiting => ("dialog-warning-symbolic", "warning", "Claude is waiting for your approval"),
            Activity::Stopped => ("process-stop-symbolic", "error", "The agent in this tab is stopped"),
            Activity::Idle | Activity::Busy => {
                self.icon.set_visible(false);
                return;
            }
        };
        self.icon.set_icon_name(Some(icon));
        self.icon.set_css_classes(&[class]);
        self.icon.set_tooltip_text(Some(tooltip));
        self.icon.set_visible(true);
    }
}
