//! The desktop tray app: an icon whose menu shows today's AI-assisted time,
//! opens the dashboard, and quits. The menu label is rebuilt every minute
//! from the local spool, so it works with or without the daemon running, and
//! nothing new leaves the computer for it to work.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tray_icon::TrayIconBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use winit::application::ApplicationHandler;
use winit::event_loop::ControlFlow;
use winit::event_loop::{ActiveEventLoop, EventLoop};

use crate::report::{day_report, format_duration};
use crate::store::Store;
use crate::{now, sync};
use chrono::Local;

/// 64x64 RGBA, drawn once with ImageMagick into `assets/tray.bin`.
const ICON: &[u8] = include_bytes!("../assets/tray.bin");
const ICON_SIZE: u32 = 64;

#[derive(Debug)]
enum UserEvent {
    Menu(MenuId),
    Refresh,
}

/// The text the menu shows: today's credited time and its biggest project.
pub fn summary(root: &Path, now: i64) -> String {
    let store = Store::new(root.to_path_buf());
    let today = Local::now().date_naive();
    let (_, _, report) = day_report(&store, today, now);
    let total = report.credited_seconds();
    if total == 0 {
        return "Today \u{00b7} nothing yet".into();
    }
    let biggest = report
        .projects
        .iter()
        .max_by_key(|(_, totals)| totals.credited_seconds)
        .map(|(project, _)| short_name(project));
    match biggest {
        Some(project) => format!(
            "Today \u{00b7} {} \u{00b7} {project}",
            format_duration(total)
        ),
        None => format!("Today \u{00b7} {}", format_duration(total)),
    }
}

/// The part of a project id worth showing in a menu line.
pub fn short_name(project: &str) -> String {
    project
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(project)
        .to_owned()
}

fn icon() -> tray_icon::Icon {
    // SAFETY of the byte layout: the file is 64*64 pixels of 4-byte RGBA
    tray_icon::Icon::from_rgba(ICON.to_vec(), ICON_SIZE, ICON_SIZE).expect("the icon is 64x64 RGBA")
}

struct TrayApp {
    store_root: Arc<std::path::PathBuf>,
    summary_item: MenuItem,
    open_item: MenuItem,
    // kept alive for as long as the tray exists
    _menu: Menu,
    _tray: tray_icon::TrayIcon,
}

impl TrayApp {
    fn refresh(&self, now: i64) {
        self.summary_item.set_text(summary(&self.store_root, now));
        self.open_item
            .set_enabled(sync::load(&self.store_root).is_some());
    }
}

impl ApplicationHandler<UserEvent> for TrayApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
        // no windows: the tray is the whole app
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Refresh => self.refresh(now()),
            UserEvent::Menu(id) if id == "quit" => event_loop.exit(),
            UserEvent::Menu(id) if id == "open" => {
                if let Some(connection) = sync::load(&self.store_root) {
                    let _ = webbrowser::open(&connection.server);
                }
            }
            UserEvent::Menu(_) => {}
        }
    }
}

pub fn run(store: &Store) -> Result<(), String> {
    let root = store.root().to_path_buf();
    // a machine without the graphics or tray libraries must get a clear
    // error, not a crash: winit panics when one is missing
    std::panic::catch_unwind(|| run_inner(&root)).map_err(|_| {
        String::from("cannot open a desktop session: a graphics or tray library is missing")
    })?
}

fn run_inner(root: &Path) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|error| format!("cannot open a desktop session: {error}"))?;

    let summary_item = MenuItem::with_id("summary", "Today \u{00b7} \u{2026}", false, None);
    let open_item = MenuItem::with_id("open", "Open dashboard", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit Pulse", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &summary_item,
        &PredefinedMenuItem::separator(),
        &open_item,
        &quit_item,
    ])
    .map_err(|error| format!("cannot build the menu: {error}"))?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip("Pulse")
        .with_icon(icon())
        .build()
        .map_err(|error| format!("cannot put Pulse in the system tray: {error}"))?;

    let menu_proxy = event_loop.create_proxy();

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event.id().clone()));
    }));

    // the label follows the spool without the daemon having to be running
    let store_root = Arc::new(root.to_path_buf());
    let refresh_proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        let _ = refresh_proxy.send_event(UserEvent::Refresh);
        loop {
            std::thread::sleep(Duration::from_secs(60));
            if refresh_proxy.send_event(UserEvent::Refresh).is_err() {
                return;
            }
        }
    });

    let mut app = TrayApp {
        store_root,
        summary_item,
        open_item,
        _menu: menu,
        _tray: tray,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|error| format!("the tray app stopped: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_are_the_last_path_segment() {
        assert_eq!(short_name("github.com/rowan/cs2-web"), "cs2-web");
        assert_eq!(short_name("local/maths-homework"), "maths-homework");
        assert_eq!(short_name("game"), "game");
        assert_eq!(short_name("a/b/"), "b");
    }
}
