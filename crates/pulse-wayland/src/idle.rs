//! Seconds since the last keyboard or mouse input on Wayland, where no
//! program can ask the compositor for it directly. Two desktop services can
//! tell us:
//!
//! - GNOME's Mutter answers `GetIdletime` on the session bus.
//! - Everyone else (KDE, sway, Hyprland and the other wlroots compositors)
//!   implements `ext-idle-notify-v1`: it says when input stopped for a whole
//!   second and when it resumed.
//!
//! Either way Pulse only ever gets this one number, never which keys were
//! pressed or where the pointer went.

use std::time::Instant;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

pub struct Idle {
    source: Source,
}

enum Source {
    Mutter(zbus::Connection),
    Notify(Box<Notify>),
    None,
}

impl Idle {
    /// Picks whichever idle timer this desktop has.
    pub fn open() -> Idle {
        if let Some(connection) = async_io::block_on(mutter_connection()) {
            return Idle {
                source: Source::Mutter(connection),
            };
        }
        match Notify::open() {
            Ok(notify) => Idle {
                source: Source::Notify(Box::new(notify)),
            },
            Err(reason) => {
                eprintln!("pulse-wayland: no idle timer: {reason}");
                Idle {
                    source: Source::None,
                }
            }
        }
    }

    pub fn backend(&self) -> &'static str {
        match self.source {
            Source::Mutter(_) => "gnome-mutter",
            Source::Notify(_) => "ext-idle-notify",
            Source::None => "none",
        }
    }

    pub fn seconds(&mut self) -> Option<u64> {
        match &mut self.source {
            Source::Mutter(connection) => async_io::block_on(mutter_seconds(connection)),
            Source::Notify(notify) => notify.seconds(),
            Source::None => None,
        }
    }
}

const MUTTER: (&str, &str, &str) = (
    "org.gnome.Mutter.IdleMonitor",
    "/org/gnome/Mutter/IdleMonitor/Core",
    "org.gnome.Mutter.IdleMonitor",
);

async fn mutter_seconds(connection: &zbus::Connection) -> Option<u64> {
    let reply = connection
        .call_method(Some(MUTTER.0), MUTTER.1, Some(MUTTER.2), "GetIdletime", &())
        .await
        .ok()?;
    let milliseconds: u64 = reply.body().deserialize().ok()?;
    Some(milliseconds / 1000)
}

/// The session bus, but only when Mutter's idle monitor answers on it.
async fn mutter_connection() -> Option<zbus::Connection> {
    let connection = zbus::Connection::session().await.ok()?;
    mutter_seconds(&connection).await?;
    Some(connection)
}

/// `ext-idle-notify-v1`, asked to tell us when a whole second passes without
/// input.
struct Notify {
    queue: EventQueue<State>,
    state: State,
    // dropping these would cancel the notification
    _notification: ExtIdleNotificationV1,
    _connection: Connection,
}

#[derive(Default)]
struct State {
    /// When the compositor said input had stopped for a second; `None` while
    /// there is input.
    idle_since: Option<Instant>,
}

/// Idle after this long without input.
const THRESHOLD_MS: u32 = 1000;

impl Notify {
    fn open() -> Result<Notify, String> {
        let connection = Connection::connect_to_env()
            .map_err(|error| format!("cannot reach the Wayland compositor: {error}"))?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection)
            .map_err(|error| format!("cannot list Wayland globals: {error}"))?;
        let handle = queue.handle();
        let notifier: ExtIdleNotifierV1 = globals
            .bind(&handle, 1..=2, ())
            .map_err(|_| "this compositor has no ext-idle-notify-v1".to_owned())?;
        let seat: WlSeat = globals
            .bind(&handle, 1..=1, ())
            .map_err(|_| "this compositor has no seat".to_owned())?;
        // version 2 can ignore idle inhibitors, which a playing video sets;
        // version 1 would then never report idle while one plays
        let notification = if notifier.version() >= 2 {
            notifier.get_input_idle_notification(THRESHOLD_MS, &seat, &handle, ())
        } else {
            notifier.get_idle_notification(THRESHOLD_MS, &seat, &handle, ())
        };
        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .map_err(|error| format!("Wayland error: {error}"))?;
        Ok(Notify {
            queue,
            state,
            _notification: notification,
            _connection: connection,
        })
    }

    fn seconds(&mut self) -> Option<u64> {
        // a round trip also delivers any idled or resumed events waiting for us
        self.queue.roundtrip(&mut self.state).ok()?;
        Some(match self.state.idle_since {
            None => 0,
            Some(since) => 1 + since.elapsed().as_secs(),
        })
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.idle_since = Some(Instant::now()),
            ext_idle_notification_v1::Event::Resumed => state.idle_since = None,
            _ => {}
        }
    }
}
