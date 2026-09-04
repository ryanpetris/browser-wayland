# Panels, taskbars and minimize

waybar and xfce4-panel run as ordinary clients of browser-wayland with working taskbars. Both are
GTK 3 applications on gtk-layer-shell; xfce's window-aware plugins go through libxfce4windowing, which
binds wlr-foreign-toplevel-management, ext-workspace, wl_seat, wl_output and xdg-output.

```sh
browser-wayland --exec 'waybar & exec foot'
browser-wayland --exec 'dbus-run-session -- sh -c "xfce4-panel & exec foot"'
```

## What the panels need and what we did

| Need | Decision |
|---|---|
| wlr-layer-shell: the bar itself, exclusive zones, menus as popups, on-demand keyboard | Smithay ships the protocol; the compositor policy is ours (below). |
| wlr-foreign-toplevel-management: waybar's `wlr/taskbar`, xfce's tasklist, windowmenu, show-desktop | Hand-written server side, protocol version 2 (v3 only adds `parent`, which we don't track). Smithay 0.7 only has the list-only ext protocol. |
| Minimize: tasklists click the active task to minimize, show-desktop minimizes everything | Unmap from the space into a list, restore on activate. Also wired to xdg `set_minimized` and X11 `WM_CHANGE_STATE`. |
| ext-workspace (xfce pager, waybar `ext/workspaces`) | Skipped: one workspace. The pager plugin stays empty. |
| idle-inhibit (waybar `idle_inhibitor`) | Skipped: the module disables itself when the global is missing. |
| Xfce's private protocols | Skipped; labwc runs Xfce without them. |

## Layer shell

- `WlrLayerShellState` with the usual delegate. Every layer surface is mapped onto the single output
  regardless of the output the client asked for.
- **Work area.** `layer_map_for_output(output).non_exclusive_zone()` is the output minus panel exclusive
  zones. Maximize fills it, fullscreen fills the output, new windows cascade inside it, `clamp_to_output`
  keeps a corner of floating windows inside it.
- **Arranging.** On a layer surface's commit the map is arranged before the initial configure so the
  configure carries the size the client's anchors give it. `LayerMap::arrange()` returns whether a
  *layer* changed size, not whether the zone moved, so `arrange_layers` compares the zone before and
  after and calls `relayout` when a panel appeared or grew. `unmap_layer` arranges by itself, so
  `layer_destroyed` always relayouts. `relayout` re-maps every window in stacking order (because
  `Space::map_element` raises) with maximized ones at the work area, fullscreen ones at the output and
  floating ones clamped.
- **Hit-testing.** Overlay and top layers before windows, bottom and background after; every surface of
  a layer is tried so an input-transparent overlay (an OSD) lets the pointer fall through to the panel
  below it.
- **Keyboard.** A click on an on-demand layer focuses it (and deactivates the windows); an exclusive
  top/overlay layer takes the keyboard when it commits (launchers). Bottom/background layers get focus
  only from a click on an empty desktop.
- **Popups.** Layer-shell popups arrive parentless and are adopted by `PopupManager` on commit; they
  are unconstrained against the layer's geometry the same way window popups are against the window's.
  Grabs work through the existing xdg grab path.
- Frame callbacks and dmabuf feedback are sent to layer surfaces after every frame; `render_output`
  already draws layers in the right order.

## Foreign toplevel management

`foreign_toplevel.rs`: one manager per taskbar client, one handle per window per manager, the `Window`
as the handle's user data. Every loop iteration a diff of title, app id and state (maximized,
minimized, activated, fullscreen) against what each taskbar was last told sends only the changed fields
followed by `done`; a window that leaves the space or the minimized list gets `closed`. Requests route
through the same functions as everything else: activate = unminimize + `focus_window`, close, maximize
and fullscreen via the fill paths (unminimizing first, since those only know mapped windows), minimize.
Requests on a handle already closed are ignored, as the protocol requires. X11 windows report their
class as the app id.

## Minimize

`State::minimized: Vec<(Window, Point)>`. `minimize` unmaps the window from the space, clears its
activated state (it can no longer be reached by `focus_window`, which only walks the space), and hands
focus to the top-most remaining window that isn't an override-redirect menu. `unminimize` maps the
window back and runs `relayout`, so a maximized or fullscreen window that was minimized across a
resize or a panel change comes back fitted. Minimized windows keep their last buffer attached, so
snapshots of them still work. In `--kiosk` mode the minimize capability isn't advertised: a nested
desktop has nowhere to come back from.

## Running the panels

- **D-Bus.** xfce4-panel needs xfconfd on a session bus; wrap it in `dbus-run-session` where there is
  none (headless boxes, containers). Its systray speaks StatusNotifier over the same bus.
- **First run.** xfce4-panel asks whether to use the default configuration. To pre-seed it, copy
  `/etc/xdg/xfce4/panel/default.xml` to
  `$XDG_CONFIG_HOME/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml` (the Dockerfile does this).
- **Icons in Xfce menus.** GTK 3 hides menu images unless `gtk-menu-images=1`; xfsettingsd normally
  sets it, we don't run one. Put it in `gtk-3.0/settings.ini` (the Docker image does so system-wide).
- **Icons on the default dock.** The default launchers reference desktop files from `xfce4-settings`
  and `xfce4-appfinder`; without those packages they show a generic gear.
- **Waybar icons.** The default config draws with Font Awesome glyphs (`otf-font-awesome`); without the
  font they render as hex boxes. Add `"wlr/taskbar"` to `modules-left` for a taskbar.
- **GTK 3 is fine.** The `GSK_RENDERER=ngl` workaround is for GTK 4's Vulkan renderer; panels aren't affected.

## Deferred

- Fullscreen windows above the top layer (a top-layer panel is drawn over a fullscreen video). Fix when
  it bites: replace `render_output` with `space_render_elements` plus our own layer order, skipping the
  top layer while a window is fullscreen, and mirror it in hit-testing.
- ext-workspace with a single workspace, if someone wants the pager.
- Restoring several minimized windows (show-desktop) maps them in list order, not the original
  stacking order.
