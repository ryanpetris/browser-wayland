//! UI elements of a window (roles, names, rectangles), read live from the toolkit's AT-SPI tree over the
//! accessibility bus of the D-Bus session this process runs in, so a script can target a button instead of
//! interpreting pixels. Coordinates come back relative to the window's geometry, like the window list.

use anyhow::{Context, Result};
use bw_core::WindowInfo;
use serde::Serialize;
use zbus::{Connection, names::BusName, proxy, proxy::CacheProperties, zvariant::OwnedObjectPath};

/// AT-SPI object reference: (bus name, object path).
type Ref = (String, OwnedObjectPath);

const COORD_WINDOW: u32 = 1;
const STATE_SHOWING: u32 = 25;
const MAX_VISITED: usize = 3000;
const MAX_ELEMENTS: usize = 500;

#[proxy(interface = "org.a11y.Bus", default_service = "org.a11y.Bus", default_path = "/org/a11y/bus")]
trait Launcher {
    fn get_address(&self) -> zbus::Result<String>;
}

#[proxy(interface = "org.a11y.atspi.Accessible", assume_defaults = false)]
trait Accessible {
    fn get_children(&self) -> zbus::Result<Vec<Ref>>;
    fn get_role(&self) -> zbus::Result<u32>;
    fn get_state(&self) -> zbus::Result<Vec<u32>>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
}

#[proxy(interface = "org.a11y.atspi.Component", assume_defaults = false)]
trait Component {
    fn get_extents(&self, coord_type: u32) -> zbus::Result<(i32, i32, i32, i32)>;
}

#[proxy(interface = "org.a11y.atspi.Application", assume_defaults = false)]
trait Application {
    #[zbus(property)]
    fn toolkit_name(&self) -> zbus::Result<String>;
}

#[derive(Serialize)]
pub struct Element {
    pub role: &'static str,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// `level`: `none` (the app isn't on the bus), `app` (on the bus, but no toplevel matches this window),
/// `frame` (the toplevel is there but empty; Chromium without --force-renderer-accessibility), `full`.
#[derive(Serialize)]
pub struct Page {
    pub level: &'static str,
    pub toolkit: Option<String>,
    pub elements: Vec<Element>,
}

pub async fn elements(win: &WindowInfo) -> Result<Page> {
    // ponytail: a fresh bus connection per request; cache one if requests ever get frequent
    let conn = a11y_bus().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let registry = ("org.a11y.atspi.Registry".to_string(), OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root")?);
    let apps = accessible(&conn, &registry).await?.get_children().await.context("accessibility registry not running")?;
    let mut app = None;
    for r in apps {
        let Ok(name) = BusName::try_from(r.0.as_str()) else { continue };
        if dbus.get_connection_unix_process_id(name).await.ok() == win.pid {
            app = Some(r);
            break;
        }
    }
    let Some(app) = app else { return Ok(Page { level: "none", toolkit: None, elements: vec![] }) };
    let toolkit = application(&conn, &app).await?.toolkit_name().await.ok();
    let mut frame = None;
    let frames: Vec<Ref> = accessible(&conn, &app).await?.get_children().await?.into_iter().filter(|r| !is_null(r)).collect();
    if frames.len() == 1 {
        frame = frames.into_iter().next();
    } else {
        for f in frames {
            if accessible(&conn, &f).await?.name().await.ok().as_deref() == Some(win.title.as_str()) {
                frame = Some(f);
                break;
            }
        }
    }
    let Some(frame) = frame else { return Ok(Page { level: "app", toolkit, elements: vec![] }) };
    let (dx, dy) = origin(component(&conn, &frame).await?.get_extents(COORD_WINDOW).await?, win);
    let children = accessible(&conn, &frame).await?.get_children().await?;
    let level = if children.is_empty() { "frame" } else { "full" };

    // depth-first in document order; a subtree that isn't showing is skipped whole
    let mut stack: Vec<Ref> = children.into_iter().rev().collect();
    let mut out = Vec::new();
    let mut visited = 0;
    while let Some(r) = stack.pop() {
        visited += 1;
        if visited > MAX_VISITED || out.len() >= MAX_ELEMENTS || is_null(&r) {
            continue;
        }
        let acc = accessible(&conn, &r).await?;
        let Ok(state) = acc.get_state().await else { continue };
        if state.first().is_none_or(|s| s & (1 << STATE_SHOWING) == 0) {
            continue;
        }
        if let Some(role) = role_name(acc.get_role().await.unwrap_or(0))
            && let Ok((x, y, w, h)) = component(&conn, &r).await?.get_extents(COORD_WINDOW).await
            && w > 0
            && h > 0
        {
            out.push(Element { role, name: acc.name().await.unwrap_or_default(), x: x - dx, y: y - dy, w, h });
        }
        if let Ok(children) = acc.get_children().await {
            stack.extend(children.into_iter().rev());
        }
    }
    Ok(Page { level, toolkit, elements: out })
}

/// Toolkits disagree on what "window coordinates" are relative to: GTK 4 uses the xdg geometry, GTK 3 and
/// Chromium the whole surface including the client-side shadow, and Firefox the surface while reporting
/// its frame at the geometry's position. A geometry-sized frame is the origin itself; otherwise the surface is.
fn origin((fx, fy, fw, fh): (i32, i32, i32, i32), win: &WindowInfo) -> (i32, i32) {
    if (fw, fh) == (win.w, win.h) { (fx, fy) } else { (win.geo_x, win.geo_y) }
}

/// The roles a script would target; containers and plain text are left out.
fn role_name(role: u32) -> Option<&'static str> {
    Some(match role {
        43 | 129 => "button",
        62 => "toggle",
        130 => "switch",
        7 => "checkbox",
        44 => "radio",
        88 => "link",
        79 => "entry",
        61 => "text",
        40 => "password",
        11 => "combobox",
        33 => "menu",
        35 | 8 | 45 => "menuitem",
        37 => "tab",
        51 => "slider",
        52 => "spinbutton",
        32 => "listitem",
        91 => "treeitem",
        48 => "scrollbar",
        83 => "heading",
        _ => return None,
    })
}

fn is_null(r: &Ref) -> bool {
    r.0.is_empty() || r.1.as_str().ends_with("/null")
}

/// `AT_SPI_BUS_ADDRESS`, else the session bus tells us where the accessibility bus is.
async fn a11y_bus() -> Result<Connection> {
    let addr = match std::env::var("AT_SPI_BUS_ADDRESS") {
        Ok(a) if !a.is_empty() => a,
        _ => {
            let session = Connection::session().await.context("no D-Bus session")?;
            LauncherProxy::new(&session).await?.get_address().await.context("no accessibility bus on the session bus")?
        }
    };
    zbus::connection::Builder::address(addr.as_str())?.build().await.context("accessibility bus")
}

async fn accessible(conn: &Connection, r: &Ref) -> Result<AccessibleProxy<'static>> {
    Ok(AccessibleProxy::builder(conn).destination(r.0.clone())?.path(r.1.clone())?.cache_properties(CacheProperties::No).build().await?)
}

async fn component(conn: &Connection, r: &Ref) -> Result<ComponentProxy<'static>> {
    Ok(ComponentProxy::builder(conn).destination(r.0.clone())?.path(r.1.clone())?.cache_properties(CacheProperties::No).build().await?)
}

async fn application(conn: &Connection, r: &Ref) -> Result<ApplicationProxy<'static>> {
    Ok(ApplicationProxy::builder(conn).destination(r.0.clone())?.path(r.1.clone())?.cache_properties(CacheProperties::No).build().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(w: i32, h: i32, geo_x: i32, geo_y: i32) -> WindowInfo {
        WindowInfo {
            id: 1, title: String::new(), app_id: String::new(), x11: false, pid: None,
            x: 0, y: 0, w, h, geo_x, geo_y, z: Some(0), maximized: false, fullscreen: false, minimized: false, focused: true, updated_ms: 0,
        }
    }

    /// Frame extents as measured from real toolkits, all with a 700×520 (or given) geometry.
    #[test]
    fn origin_per_toolkit() {
        assert_eq!(origin((0, 0, 700, 520), &win(700, 520, 0, 0)), (0, 0)); // GTK 4: frame == geometry
        assert_eq!(origin((0, 0, 952, 799), &win(900, 747, 26, 23)), (26, 23)); // GTK 3: frame == surface
        assert_eq!(origin((0, 0, 945, 1060), &win(921, 1035, 12, 10)), (12, 10)); // Chromium: frame == surface
        assert_eq!(origin((26, 23, 1280, 972), &win(1280, 972, 26, 23)), (26, 23)); // Firefox: geometry-sized frame at the offset
    }
}
