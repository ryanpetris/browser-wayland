# MCP, input and the skill documents

Coding agents drive the desktop through the same operations as the HTTP API, so an agent can find a
button by name, click it, type, and check the result without interpreting pixels. This document covers
the design; [protocol.md](protocol.md) has the wire shapes and `skills/browser-wayland/reference.md` the
generated schemas.

## Decisions

| Question | Decision |
|---|---|
| Protocol | MCP over Streamable HTTP at `/mcp` on the existing server, with the `rmcp` SDK. No second port or process; works remotely and from the container. Agents that only speak stdio use the standard `mcp-remote` bridge. |
| Auth | The bearer tokens, through the same middleware as `/api`; `401` otherwise. The middleware tags the request with which token it carried (rmcp hands tools the HTTP request parts), so the tools that act answer `read-only token` as a tool error to the viewer token. rmcp's host allow-list (DNS-rebinding protection) is off because the tokens already gate every request. No OAuth. |
| Shape of the tools | One tool per thing an agent wants to say, sixteen in all, each a few lines calling an `App` method. Arguments use the API's own types where they exist (`Button`, the control ops) so the vocabulary is shared. |
| Results | JSON as text content for lists and elements, `image/png` content for snapshots, `ok` for fire-and-forget actions, and `isError` results carrying the API's error text so the model can react. |
| Documentation | The manual is the server's `instructions` at `initialize` and both skill files are MCP resources, so a client gets the documentation with the connection. |
| Screenshots for models | `snapshot` and `screenshot` default to a scale that fits the long side in about 1600 device pixels; both take an explicit `scale`. |

## Input

The compositor could always move the pointer, click and press keys: that is how the viewer works. What
was missing was a way in other than the viewer's binary WebSocket. `InputMsg` in `bw-core` is that way:
`move`, `click`, `button`, `scroll`, `key`, `text`, served as `POST /api/input` and as tools.

- The server only checks that a named window exists (for the `404`) and forwards the message as one
  `Command::Input`. The compositor resolves window-relative coordinates against the geometry it has at
  that moment and emits a click's motion and button events in one go, so neither a moving window nor a
  human's pointer motion arriving in between can redirect it. Coordinates are output logical pixels or,
  with a window id, relative to that window's geometry, the origin element rectangles use.
- Keys are resolved where the keymap is. `key_for` in `input.rs` scans the active layout for the
  keycode producing a keysym at levels 0 to 3 and `level_mods` adds Shift, AltGr or both for the level,
  the convention of four-level layouts, so `text` types any character the layout has (`@` on a German
  layout is AltGr+q) and `key` accepts every xkb keysym name plus friendly modifier names. A lone letter
  in a chord is lowercased first, so `ctrl+T` is Ctrl+T, not Ctrl+Shift+T.
- A chord aborts as a whole when a keysym has no key in the layout, rather than pressing the modifiers
  alone; a character `text` can't produce is skipped with a warning. Press and release go out back to
  back; GTK, Firefox and Chromium process them in order, so no pacing was needed.
- `tap` releases only the keys it pressed itself: the `key()` guard drops a press for a key a viewer is
  already holding, and that key stays held for the viewer afterwards.

## Skill documents

Two files, both compiled in with `include_str!` and served at `/skill/`:

- `SKILL.md`, written by hand for an agent: the loop (windows, elements, act, read again, snapshot to
  confirm), input details, what the status codes mean, and the things that surprise people.
- `reference.md`, generated: the route table, the JSON schemas of `WindowInfo`, `ControlMsg`,
  `InputMsg` and the elements page (from `schemars`, the same derive rmcp uses for tool arguments),
  and every MCP tool with its description and input schema (from the tool router). The
  `reference_is_current` test compares the checked-in file with what the code generates, so it is
  regenerated with `UPDATE_REFERENCE=1 cargo test -p bw-server reference` (after `make web`, which
the embedded viewer needs) and cannot silently drift.

## Verification

- A scripted MCP handshake with curl against a test compositor: `initialize` (server info,
  capabilities, instructions), `tools/list` (16 tools), `resources/list` and `resources/read`, and
  `tools/call` for `windows`, `elements`, `click`, `type` (text appeared in the editor) and `snapshot`
  (a PNG of the window's size), plus a tool error for an unknown window and `401` without the token.
- The input route through curl: click into an editor, type text with punctuation and capitals, `Return`,
  `ctrl+a`; the snapshot showed the typed lines selected.
- Key resolution has a unit test on the `us` keymap (letters, Shift for capitals and `plus`, `ctrl`,
  `Return`, `F5`).
