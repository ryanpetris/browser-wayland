//! UI elements of a window (roles, names, rectangles), read live from the toolkit's AT-SPI tree over the
//! accessibility bus of the D-Bus session this process runs in, so a script can target a button instead of
//! interpreting pixels. Coordinates come back relative to the window's geometry, like the window list.

use anyhow::{Context, Result};
use bw_core::WindowInfo;
use schemars::JsonSchema;
use serde::Serialize;
use zbus::{Connection, names::BusName, proxy, proxy::CacheProperties, zvariant::OwnedObjectPath};

/// AT-SPI object reference: (bus name, object path).
type Ref = (String, OwnedObjectPath);

const COORD_WINDOW: u32 = 1;
const ROLE_MENU: u32 = 33;
const ROLE_WINDOW: u32 = 69;
const ROLE_DOCUMENT_WEB: u32 = 95;
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

#[derive(Serialize, JsonSchema)]
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
#[derive(Serialize, JsonSchema)]
pub struct Page {
    pub level: &'static str,
    pub toolkit: Option<String>,
    pub elements: Vec<Element>,
}

/// The window's elements: the application's from its accessibility tree, then the compositor's own
/// decorations (title bar and buttons, above the geometry at negative `y`) when it draws them.
/// `scale` is the output scale: Chromium reports its web content in device pixels (its own UI in logical ones).
pub async fn elements(win: &WindowInfo, scale: f64) -> Result<Page> {
    let mut page = app_elements(win, scale).await?;
    if win.decoration > 0 {
        use bw_core::decoration::{BAR, BUTTON, buttons};
        page.elements.push(Element { role: "title bar", name: win.title.clone(), x: 0, y: -BAR, w: win.w, h: BAR });
        page.elements.extend(buttons(win.w).map(|(b, x)| Element { role: "push button", name: b.name(win.maximized).into(), x, y: -BAR, w: BUTTON, h: BAR }));
    }
    Ok(page)
}

async fn app_elements(win: &WindowInfo, scale: f64) -> Result<Page> {
    // ponytail: a fresh bus connection per request; cache one if requests ever get frequent
    let conn = a11y_bus().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let registry = ("org.a11y.atspi.Registry".to_string(), OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root")?);
    let apps = proxy::<AccessibleProxy>(&conn, &registry).await?.get_children().await.context("accessibility registry not running")?;
    let none = Page { level: "none", toolkit: None, elements: vec![] };
    let Some(pid) = win.pid else { return Ok(none) };
    let mut app = None;
    for r in apps {
        let Ok(name) = BusName::try_from(r.0.as_str()) else { continue };
        if dbus.get_connection_unix_process_id(name).await.ok() == Some(pid) {
            app = Some(r);
            break;
        }
    }
    let Some(app) = app else { return Ok(none) };
    let toolkit = proxy::<ApplicationProxy>(&conn, &app).await?.toolkit_name().await.ok();
    // The application's toplevels: real windows (frame, dialog) and, in GTK 3, one borderless `window`
    // per open menu. The frame is the one named like the window; a lone toplevel needs no match.
    let mut tops = Vec::new();
    for r in proxy::<AccessibleProxy>(&conn, &app).await?.get_children().await?.into_iter().filter(|r| !is_null(r)) {
        let acc = proxy::<AccessibleProxy>(&conn, &r).await?;
        tops.push((r, acc.get_role().await.ok() == Some(ROLE_WINDOW), acc.name().await.unwrap_or_default()));
    }
    let mut frames: Vec<&Ref> = tops.iter().filter(|t| !t.1).map(|t| &t.0).collect();
    if frames.is_empty() {
        frames = tops.iter().map(|t| &t.0).collect();
    }
    let frame = match frames.len() {
        1 => Some(frames[0].clone()),
        _ => tops.iter().find(|t| t.2 == win.title).map(|t| t.0.clone()),
    };
    let Some(frame) = frame else { return Ok(Page { level: "app", toolkit, elements: vec![] }) };
    let (dx, dy) = origin(proxy::<ComponentProxy>(&conn, &frame).await?.get_extents(COORD_WINDOW).await?, win);
    let children = proxy::<AccessibleProxy>(&conn, &frame).await?.get_children().await?;
    let level = if children.is_empty() && win.popups.is_empty() { "frame" } else { "full" };
    // menu windows are walked only while this window has popups open; their nodes count only once placed
    let menus = tops.iter().filter(|t| t.1 && t.0 != frame && !win.popups.is_empty()).map(|t| t.0.clone());

    // depth-first in document order; a subtree that isn't showing is skipped whole. The flag marks
    // Chromium web content, whose extents are in device pixels (everything else is logical). The shift
    // places an open menu: toolkits report its items relative to the menu's own popup surface, so a menu
    // node with the size of an open popup gets that popup's position for itself and its subtree.
    let chromium = toolkit.as_deref() == Some("Chromium");
    let mut used = vec![false; win.popups.len()];
    // (node, in Chromium web content, shift onto a popup, inside a menu window)
    let mut stack: Vec<(Ref, bool, Option<(i32, i32)>, bool)> = menus.rev().map(|r| (r, false, None, true)).collect();
    stack.extend(children.into_iter().rev().map(|r| (r, false, None, false)));
    let mut out = Vec::new();
    let mut visited = 0;
    while let Some((r, device, mut shift, in_menu)) = stack.pop() {
        visited += 1;
        if visited > MAX_VISITED || out.len() >= MAX_ELEMENTS {
            break;
        }
        if is_null(&r) {
            continue;
        }
        let acc = proxy::<AccessibleProxy>(&conn, &r).await?;
        let Ok(state) = acc.get_state().await else { continue };
        if state.first().is_none_or(|s| s & (1 << STATE_SHOWING) == 0) {
            continue;
        }
        let role = acc.get_role().await.unwrap_or(0);
        let s = if device { scale } else { 1.0 };
        let ext = extents(&conn, &r, s).await.ok().filter(|e| e.2 > 0 && e.3 > 0);
        let mut matched = false;
        if let Some(name) = role_name(role)
            && let Some((x, y, w, h)) = ext
        {
            // a menu in its own window (context menu): the menu node itself has the popup's size
            if role == ROLE_MENU && let Some(i) = win.popups.iter().enumerate().position(|(i, p)| (p.2, p.3) == (w, h) && !used[i]) {
                used[i] = true;
                shift = Some((win.popups[i].0 - x, win.popups[i].1 - y));
                matched = true;
            }
            let (x, y) = match shift {
                Some((sx, sy)) => (x + sx, y + sy),
                None => (x - dx, y - dy),
            };
            if shift.is_some() || !in_menu {
                out.push(Element { role: name, name: acc.name().await.unwrap_or_default(), x, y, w, h });
            }
        }
        if let Ok(children) = acc.get_children().await {
            let mut child_shift = shift;
            // a menubar menu: GTK 3 hangs the items straight off the menubar item, in the coordinates of the
            // items' popup, so they fall outside the item; as a group they have the popup's width and about
            // its height
            if role == ROLE_MENU && !matched && let Some((mx, my, mw, mh)) = ext {
                let mut union: Option<(i32, i32, i32, i32)> = None; // x0, y0, x1, y1
                for c in &children {
                    if let Ok((cx, cy, cw, ch)) = extents(&conn, c, s).await && cw > 0 && ch > 0 && cx.abs() < 1 << 20 && cy.abs() < 1 << 20 {
                        union = Some(union.map_or((cx, cy, cx + cw, cy + ch), |u| (u.0.min(cx), u.1.min(cy), u.2.max(cx + cw), u.3.max(cy + ch))));
                    }
                }
                if let Some((x0, y0, x1, y1)) = union
                    && !(x0 >= mx && y0 >= my && x1 <= mx + mw && y1 <= my + mh)
                    && let Some(i) = win.popups.iter().enumerate().position(|(i, p)| !used[i] && p.2 == x1 - x0 && (0..=16).contains(&(p.3 - (y1 - y0))))
                {
                    used[i] = true;
                    let p = win.popups[i];
                    child_shift = Some((p.0 - x0, p.1 + (p.3 - (y1 - y0)) / 2 - y0));
                }
            }
            let device = device || (chromium && role == ROLE_DOCUMENT_WEB);
            stack.extend(children.into_iter().rev().map(|r| (r, device, child_shift, in_menu)));
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

/// Window-relative extents in logical pixels (`scale` > 1 for Chromium web content, which reports device pixels).
async fn extents(conn: &Connection, r: &Ref, scale: f64) -> Result<(i32, i32, i32, i32)> {
    let (x, y, w, h) = proxy::<ComponentProxy>(conn, r).await?.get_extents(COORD_WINDOW).await?;
    let px = |v: i32| (v as f64 / scale).round() as i32;
    Ok((px(x), px(y), px(w), px(h)))
}

/// A proxy for one object; no property cache, so hundreds of short-lived proxies don't each subscribe to signals.
async fn proxy<P: zbus::proxy::ProxyImpl<'static> + zbus::proxy::Defaults + From<zbus::Proxy<'static>>>(conn: &Connection, r: &Ref) -> Result<P> {
    Ok(zbus::proxy::Builder::<P>::new(conn).destination(r.0.clone())?.path(r.1.clone())?.cache_properties(CacheProperties::No).build().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(w: i32, h: i32, geo_x: i32, geo_y: i32) -> WindowInfo {
        WindowInfo {
            id: 1, title: String::new(), app_id: String::new(), x11: false, pid: None,
            x: 0, y: 0, w, h, geo_x, geo_y, popups: vec![], decoration: 0, z: Some(0), maximized: false, fullscreen: false, minimized: false, focused: true, updated_ms: 0,
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
