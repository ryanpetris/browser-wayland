# browser-wayland API and MCP reference

Generated from the code (`UPDATE_REFERENCE=1 cargo test -p bw-server reference`); do not edit.

## HTTP API

Every `/api` request carries `Authorization: Bearer <token>`; `401` (empty body) otherwise. The
viewer token (the server prints it as "view only") reads: the acting routes and tools answer `403`
`read-only token` to it. The
statuses in the table come with a JSON body `{"error": "..."}`. A request body the server can't read is
rejected before that with a plain-text message: `400` invalid JSON, `415` missing
`Content-Type: application/json`, `422` wrong shape. Coordinates are logical pixels.

| Method and path | Body or query | Result |
|---|---|---|
| `GET /api/windows` | | JSON array of **Window** |
| `GET /api/codecs` | | JSON array of `{codec, hardware}`: what this server encodes, in the order Auto prefers |
| `GET /api/applications` | | JSON array of **Application**: the installed launchers, for `launch` |
| `GET /api/applications/{id}/icon` | | the application's icon, SVG or PNG; `404` none |
| `GET /api/windows/{id}/icon` | | the window's icon (its own, else its launcher's), SVG or PNG; `404` none |
| `GET /api/files` | | JSON array of **File**: the transfer folder (dropped files, downloads), newest first |
| `PUT /api/files/{name}` | the file's bytes | saved in the folder under `name`, or `name (2)` if taken; `201` with `{"name"}` |
| `GET /api/files/{name}` | | the file, as an attachment; `404` |
| `DELETE /api/files/{name}` | | `204`; `404` |
| `GET /api/notifications` | | JSON array of **Notification**: what applications reported and the viewers show |
| `POST /api/notifications/{id}` | `{"action": "default" \| "<key>"}`, or `{}` to dismiss | click, invoke an action of, or dismiss a notification; `202`, `404` |
| `GET /api/notifications/{id}/icon` | | the notification's picture (the application's, else its launcher's); `404` none |
| `GET /api/windows/{id}/elements` | | **Elements**; `501` without `--elements`, `503` tree unreadable, `404` unknown window |
| `GET /api/windows/{id}/snapshot.png?scale=` | `scale` 0.05–2, default 1 | PNG of the window; `404`, `429` another snapshot in flight, `500` render failed, `503` |
| `GET /api/screenshot.png?scale=` | `scale` 0.05–2, default 1 | PNG of the whole output; `429`, `500`, `503` as for a window |
| `POST /api/control` | **Control** | `202`; fire-and-forget; `404` unknown application (`launch`); `503` compositor gone |
| `POST /api/input` | **Input** | `202`, with `{"warning": …}` when a click aims past the desktop's edge at an X11 window (Xwayland pins it to the edge); `404` unknown window; `503` compositor gone |
| `GET /api/clipboard` | | what an application last copied: `text/plain`, or `image/png`; `204` before any |
| `PUT /api/clipboard` | UTF-8 text body, or a PNG with `Content-Type: image/png` | becomes the desktop clipboard; `202`; `413` over 1 MiB (text) or 16 MiB (PNG) |
| `POST /api/token/rotate` | | `{"token": …, "viewer_token": …}`: new tokens replace both at once (files, viewers, API); the server prints the new URLs |
| `POST /mcp` | MCP Streamable HTTP | the tools below |
| `GET /skill/SKILL.md`, `GET /skill/reference.md` | no token needed | this documentation |

## Window

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "WindowInfo",
  "description": "One window as the desktop API reports it.",
  "type": "object",
  "properties": {
    "app_id": {
      "description": "X11: the WM_CLASS",
      "type": "string"
    },
    "content": {
      "description": "what the client says it shows (content-type-v1): `photo`, `video` or `game`; null for ordinary windows",
      "type": [
        "string",
        "null"
      ]
    },
    "decoration": {
      "description": "height of the compositor's title bar above the geometry (the `decoration` module has its layout); 0 when the client draws its own",
      "type": "integer",
      "format": "int32"
    },
    "focused": {
      "type": "boolean"
    },
    "fullscreen": {
      "type": "boolean"
    },
    "geo_x": {
      "description": "where the geometry sits inside the client's surface (its shadow margin); 0 for X11",
      "type": "integer",
      "format": "int32"
    },
    "geo_y": {
      "type": "integer",
      "format": "int32"
    },
    "h": {
      "type": "integer",
      "format": "int32"
    },
    "icon": {
      "description": "the icon name the client set (xdg-toplevel-icon); the picture is at `/api/windows/{id}/icon`, which falls back to the launcher's icon",
      "type": [
        "string",
        "null"
      ]
    },
    "id": {
      "type": "integer",
      "format": "uint64",
      "minimum": 0
    },
    "maximized": {
      "type": "boolean"
    },
    "minimized": {
      "type": "boolean"
    },
    "pid": {
      "type": [
        "integer",
        "null"
      ],
      "format": "uint32",
      "minimum": 0
    },
    "popups": {
      "description": "open popups (menus, combo lists) as `(x, y, w, h)` relative to the geometry; Wayland only",
      "type": "array",
      "items": {
        "type": "array",
        "maxItems": 4,
        "minItems": 4,
        "prefixItems": [
          {
            "type": "integer",
            "format": "int32"
          },
          {
            "type": "integer",
            "format": "int32"
          },
          {
            "type": "integer",
            "format": "int32"
          },
          {
            "type": "integer",
            "format": "int32"
          }
        ]
      }
    },
    "title": {
      "type": "string"
    },
    "updated_ms": {
      "description": "last commit, ms on the compositor clock",
      "type": "integer",
      "format": "uint64",
      "minimum": 0
    },
    "w": {
      "type": "integer",
      "format": "int32"
    },
    "x": {
      "description": "xdg geometry in logical px",
      "type": "integer",
      "format": "int32"
    },
    "x11": {
      "type": "boolean"
    },
    "y": {
      "type": "integer",
      "format": "int32"
    },
    "z": {
      "description": "stacking index, 0 = bottom; `None` while minimized",
      "type": [
        "integer",
        "null"
      ],
      "format": "uint32",
      "minimum": 0
    }
  },
  "required": [
    "id",
    "title",
    "app_id",
    "x11",
    "x",
    "y",
    "w",
    "h",
    "geo_x",
    "geo_y",
    "popups",
    "decoration",
    "maximized",
    "fullscreen",
    "minimized",
    "focused",
    "updated_ms"
  ]
}
```

## Application

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "AppInfo",
  "description": "One launcher, as the menu shows it.",
  "type": "object",
  "properties": {
    "categories": {
      "description": "The entry's categories (`Network`, `Office`, ...), for grouping.",
      "type": "array",
      "items": {
        "type": "string"
      }
    },
    "comment": {
      "type": [
        "string",
        "null"
      ]
    },
    "id": {
      "description": "The desktop file's name without `.desktop`; what `launch` takes.",
      "type": "string"
    },
    "name": {
      "type": "string"
    }
  },
  "required": [
    "id",
    "name",
    "categories"
  ]
}
```

## Notification

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Notification",
  "description": "One notification, as the viewers and `GET /api/notifications` see it.",
  "type": "object",
  "properties": {
    "actions": {
      "description": "`[key, label]` pairs the application offers; a plain click means `default` when that key is among them",
      "type": "array",
      "items": {
        "type": "array",
        "maxItems": 2,
        "minItems": 2,
        "prefixItems": [
          {
            "type": "string"
          },
          {
            "type": "string"
          }
        ]
      }
    },
    "app": {
      "description": "the application's name as it gave it",
      "type": "string"
    },
    "body": {
      "type": "string"
    },
    "icon": {
      "description": "whether `GET /api/notifications/{id}/icon` has a picture",
      "type": "boolean"
    },
    "id": {
      "type": "integer",
      "format": "uint32",
      "minimum": 0
    },
    "rev": {
      "description": "counts up when the application replaces the notification under the same id",
      "type": "integer",
      "format": "uint64",
      "minimum": 0
    },
    "summary": {
      "type": "string"
    },
    "timeout_ms": {
      "description": "how long it is shown, ms; 0 means until closed",
      "type": "integer",
      "format": "uint32",
      "minimum": 0
    }
  },
  "required": [
    "id",
    "rev",
    "app",
    "summary",
    "body",
    "icon",
    "actions",
    "timeout_ms"
  ]
}
```

## File

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "FileInfo",
  "description": "One file in the folder.",
  "type": "object",
  "properties": {
    "modified_ms": {
      "description": "last modification, ms since the epoch",
      "type": "integer",
      "format": "uint64",
      "minimum": 0
    },
    "name": {
      "type": "string"
    },
    "size": {
      "type": "integer",
      "format": "uint64",
      "minimum": 0
    }
  },
  "required": [
    "name",
    "size",
    "modified_ms"
  ]
}
```

## Control

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "ControlMsg",
  "description": "`{\"id\":3,\"op\":\"move\",\"x\":10,\"y\":20}`, `{\"op\":\"spawn\",\"cmd\":\"foot\"}`.",
  "type": "object",
  "properties": {
    "id": {
      "type": "integer",
      "format": "uint64",
      "default": 0,
      "minimum": 0
    }
  },
  "oneOf": [
    {
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "activate"
        }
      },
      "required": [
        "op"
      ]
    },
    {
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "close"
        }
      },
      "required": [
        "op"
      ]
    },
    {
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "minimize"
        }
      },
      "required": [
        "op"
      ]
    },
    {
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "unminimize"
        }
      },
      "required": [
        "op"
      ]
    },
    {
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "maximize"
        }
      },
      "required": [
        "op"
      ]
    },
    {
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "unmaximize"
        }
      },
      "required": [
        "op"
      ]
    },
    {
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "fullscreen"
        }
      },
      "required": [
        "op"
      ]
    },
    {
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "unfullscreen"
        }
      },
      "required": [
        "op"
      ]
    },
    {
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "move"
        },
        "x": {
          "type": "integer",
          "format": "int32"
        },
        "y": {
          "type": "integer",
          "format": "int32"
        }
      },
      "required": [
        "op",
        "x",
        "y"
      ]
    },
    {
      "type": "object",
      "properties": {
        "h": {
          "type": "integer",
          "format": "int32"
        },
        "op": {
          "type": "string",
          "const": "resize"
        },
        "w": {
          "type": "integer",
          "format": "int32"
        }
      },
      "required": [
        "op",
        "w",
        "h"
      ]
    },
    {
      "description": "`sh -c`, with the same environment as `--exec`",
      "type": "object",
      "properties": {
        "cmd": {
          "type": "string"
        },
        "op": {
          "type": "string",
          "const": "spawn"
        }
      },
      "required": [
        "op",
        "cmd"
      ]
    },
    {
      "description": "Start an installed application by its id from `GET /api/applications` (its `.desktop` file's name)",
      "type": "object",
      "properties": {
        "app": {
          "type": "string"
        },
        "op": {
          "type": "string",
          "const": "launch"
        }
      },
      "required": [
        "op",
        "app"
      ]
    },
    {
      "description": "End browser-wayland: every window closes with it",
      "type": "object",
      "properties": {
        "op": {
          "type": "string",
          "const": "quit"
        }
      },
      "required": [
        "op"
      ]
    }
  ]
}
```

## Input

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "InputMsg",
  "description": "One input action (`POST /api/input`, MCP tools). Coordinates are logical pixels on the output, or\nrelative to a window's geometry when `window` is given, like element rectangles.",
  "oneOf": [
    {
      "description": "Move the pointer.",
      "type": "object",
      "properties": {
        "type": {
          "type": "string",
          "const": "move"
        },
        "window": {
          "type": [
            "integer",
            "null"
          ],
          "format": "uint64",
          "default": null,
          "minimum": 0
        },
        "x": {
          "type": "number",
          "format": "double"
        },
        "y": {
          "type": "number",
          "format": "double"
        }
      },
      "required": [
        "type",
        "x",
        "y"
      ]
    },
    {
      "description": "Move the pointer there and click `count` times (default 1).",
      "type": "object",
      "properties": {
        "button": {
          "$ref": "#/$defs/Button"
        },
        "count": {
          "type": [
            "integer",
            "null"
          ],
          "format": "uint32",
          "default": null,
          "maximum": 3,
          "minimum": 1
        },
        "type": {
          "type": "string",
          "const": "click"
        },
        "window": {
          "type": [
            "integer",
            "null"
          ],
          "format": "uint64",
          "default": null,
          "minimum": 0
        },
        "x": {
          "type": "number",
          "format": "double"
        },
        "y": {
          "type": "number",
          "format": "double"
        }
      },
      "required": [
        "type",
        "x",
        "y"
      ]
    },
    {
      "description": "Press or release a button where the pointer is (drags).",
      "type": "object",
      "properties": {
        "button": {
          "$ref": "#/$defs/Button"
        },
        "pressed": {
          "type": "boolean"
        },
        "type": {
          "type": "string",
          "const": "button"
        }
      },
      "required": [
        "type",
        "button",
        "pressed"
      ]
    },
    {
      "description": "Scroll by wheel lines; positive `dy` scrolls down.",
      "type": "object",
      "properties": {
        "dx": {
          "type": "number",
          "format": "double",
          "default": 0.0
        },
        "dy": {
          "type": "number",
          "format": "double",
          "default": 0.0
        },
        "type": {
          "type": "string",
          "const": "scroll"
        }
      },
      "required": [
        "type"
      ]
    },
    {
      "description": "A key chord, `+`-separated: `ctrl+shift+t`, `Return`, `alt+F4`. Modifiers first, the key last; all released after.",
      "type": "object",
      "properties": {
        "keys": {
          "type": "string"
        },
        "type": {
          "type": "string",
          "const": "key"
        }
      },
      "required": [
        "type",
        "keys"
      ]
    },
    {
      "description": "Type text through the keyboard layout.",
      "type": "object",
      "properties": {
        "text": {
          "type": "string"
        },
        "type": {
          "type": "string",
          "const": "text"
        }
      },
      "required": [
        "type",
        "text"
      ]
    }
  ],
  "$defs": {
    "Button": {
      "type": "string",
      "enum": [
        "left",
        "right",
        "middle"
      ]
    }
  }
}
```

## Elements

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Page",
  "description": "`level`: `none` (the app isn't on the bus), `app` (on the bus, but no toplevel matches this window),\n`frame` (the toplevel is there but empty; Chromium without --force-renderer-accessibility), `full`.",
  "type": "object",
  "properties": {
    "elements": {
      "type": "array",
      "items": {
        "$ref": "#/$defs/Element"
      }
    },
    "level": {
      "type": "string"
    },
    "toolkit": {
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "level",
    "elements"
  ],
  "$defs": {
    "Element": {
      "type": "object",
      "properties": {
        "h": {
          "type": "integer",
          "format": "int32"
        },
        "name": {
          "type": "string"
        },
        "role": {
          "type": "string"
        },
        "w": {
          "type": "integer",
          "format": "int32"
        },
        "x": {
          "type": "integer",
          "format": "int32"
        },
        "y": {
          "type": "integer",
          "format": "int32"
        }
      },
      "required": [
        "role",
        "name",
        "x",
        "y",
        "w",
        "h"
      ]
    }
  }
}
```

## MCP tools

Streamable HTTP at `/mcp`, same bearer token. Failures come back as tool errors with the same text as the API.

### `applications`

The applications installed on the desktop (its .desktop launchers): id, name, comment, categories. `launch` starts one.

```json
{
  "properties": {},
  "type": "object"
}
```

### `button`

Press or release a pointer button where the pointer is (for drags: press, move_pointer, release).

```json
{
  "$defs": {
    "Button": {
      "enum": [
        "left",
        "right",
        "middle"
      ],
      "type": "string"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "button": {
      "$ref": "#/$defs/Button"
    },
    "pressed": {
      "type": "boolean"
    }
  },
  "required": [
    "button",
    "pressed"
  ],
  "type": "object"
}
```

### `click`

Move the pointer there and click. With `window`, x y are relative to that window, e.g. the centre of an element's rectangle.

```json
{
  "$defs": {
    "Button": {
      "enum": [
        "left",
        "right",
        "middle"
      ],
      "type": "string"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "button": {
      "$ref": "#/$defs/Button",
      "description": "left (default), right or middle"
    },
    "count": {
      "default": null,
      "description": "1 (default) to 3 clicks",
      "format": "uint32",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "window": {
      "default": null,
      "description": "With a window id, `x`/`y` are relative to that window (as element rectangles are); else output pixels.",
      "format": "uint64",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "x": {
      "format": "double",
      "type": "number"
    },
    "y": {
      "format": "double",
      "type": "number"
    }
  },
  "required": [
    "x",
    "y"
  ],
  "type": "object"
}
```

### `clipboard_read`

The last text a desktop application copied to the clipboard (empty if none yet). An image says so; GET /api/clipboard returns its bytes.

```json
{
  "properties": {},
  "type": "object"
}
```

### `clipboard_write`

Put text on the desktop clipboard, for pasting into an application (images go through PUT /api/clipboard).

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "text": {
      "description": "Becomes the desktop clipboard (text only, up to 1 MiB).",
      "type": "string"
    }
  },
  "required": [
    "text"
  ],
  "type": "object"
}
```

### `elements`

The UI elements of a window (buttons, links, text fields, menu items, tabs, ...): role, name, and x y w h relative to the window's own x y. `level` full means the list is complete; none/app/frame mean the application exposes nothing (use a snapshot).

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "window": {
      "description": "Window id from `windows`.",
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "window"
  ],
  "type": "object"
}
```

### `files`

The files in the desktop's transfer folder (what was dropped on the page, and what the desktop put there for download): name, size, modified_ms. GET /api/files/{name} downloads one.

```json
{
  "properties": {},
  "type": "object"
}
```

### `key`

Press a key chord and release it: `ctrl+s`, `ctrl+shift+t`, `alt+F4`, `Return`, `Escape`, `Tab`, `Down`, `Prior`, `F5`. Goes to the focused window.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "keys": {
      "description": "A chord, `+`-separated: `ctrl+shift+t`, `alt+F4`, `Return`, `Escape`, `Down`, `Prior`, `F5`.",
      "type": "string"
    }
  },
  "required": [
    "keys"
  ],
  "type": "object"
}
```

### `launch`

Start an installed application by its id from `applications`, as a click in the menu would. Its window appears in `windows` after a moment.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "app": {
      "description": "An `id` from `applications`.",
      "type": "string"
    }
  },
  "required": [
    "app"
  ],
  "type": "object"
}
```

### `move_pointer`

Move the pointer without clicking (hover, or the middle of a drag).

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "window": {
      "default": null,
      "description": "With a window id, `x`/`y` are relative to that window (as element rectangles are); else output pixels.",
      "format": "uint64",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "x": {
      "format": "double",
      "type": "number"
    },
    "y": {
      "format": "double",
      "type": "number"
    }
  },
  "required": [
    "x",
    "y"
  ],
  "type": "object"
}
```

### `move_window`

Move a floating window's geometry to x y (output logical px).

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "window": {
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    },
    "x": {
      "description": "New position of the window's geometry in output logical pixels.",
      "format": "int32",
      "type": "integer"
    },
    "y": {
      "format": "int32",
      "type": "integer"
    }
  },
  "required": [
    "window",
    "x",
    "y"
  ],
  "type": "object"
}
```

### `notifications`

The desktop notifications currently shown (id, app, summary, body, actions, timeout_ms): what applications reported. POST /api/notifications/{id} with {"action": key} acts on one, {} dismisses it.

```json
{
  "properties": {},
  "type": "object"
}
```

### `resize_window`

Resize a floating window's geometry to w h (logical px).

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "h": {
      "format": "int32",
      "type": "integer"
    },
    "w": {
      "description": "New size of the window's geometry in logical pixels.",
      "format": "int32",
      "type": "integer"
    },
    "window": {
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "window",
    "w",
    "h"
  ],
  "type": "object"
}
```

### `screenshot`

PNG of the whole output (panels included, pointer excluded), scaled to fit about 1600 px unless `scale` is given.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "scale": {
      "description": "0.05..=2 relative to the output scale; default fits the long side in about 1600 px.",
      "format": "double",
      "type": [
        "number",
        "null"
      ]
    }
  },
  "type": "object"
}
```

### `scroll`

Scroll the wheel under the pointer by lines; positive dy scrolls down.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "dx": {
      "default": 0.0,
      "description": "Wheel lines; positive scrolls right.",
      "format": "double",
      "type": "number"
    },
    "dy": {
      "default": 0.0,
      "description": "Wheel lines; positive scrolls down.",
      "format": "double",
      "type": "number"
    }
  },
  "type": "object"
}
```

### `snapshot`

PNG of one window's own buffers (works for covered and minimized windows), popups included.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "scale": {
      "default": null,
      "description": "0.05..=2 relative to the output scale; default fits the long side in about 1600 px.",
      "format": "double",
      "type": [
        "number",
        "null"
      ]
    },
    "window": {
      "description": "Window id from `windows`.",
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "window"
  ],
  "type": "object"
}
```

### `spawn`

Start a program as a client of this desktop (`sh -c cmd`). Its window appears in `windows` after a moment.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "cmd": {
      "description": "Shell command, run with `sh -c` as a client of this desktop.",
      "type": "string"
    }
  },
  "required": [
    "cmd"
  ],
  "type": "object"
}
```

### `type`

Type text into the focused field through the keyboard layout (click it first). `\n` is Return.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "text": {
      "description": "Typed through the keyboard layout into the focused field; `\\n` is Return.",
      "type": "string"
    }
  },
  "required": [
    "text"
  ],
  "type": "object"
}
```

### `window_control`

Change a window's state: activate (raise, focus, restore), close, minimize, unminimize, maximize, unmaximize, fullscreen, unfullscreen. Fire-and-forget; check `windows` afterwards.

```json
{
  "$defs": {
    "WindowOp": {
      "enum": [
        "activate",
        "close",
        "minimize",
        "unminimize",
        "maximize",
        "unmaximize",
        "fullscreen",
        "unfullscreen"
      ],
      "type": "string"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "op": {
      "$ref": "#/$defs/WindowOp"
    },
    "window": {
      "description": "Window id from `windows`.",
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "window",
    "op"
  ],
  "type": "object"
}
```

### `windows`

The windows on the desktop: id, title, app_id, icon (name the client set; its picture is at GET /api/windows/{id}/icon), content (video/game/photo when the client says so), pid, geometry x y w h (logical px), stacking z, maximized/fullscreen/minimized/focused, updated_ms (last redraw), popups (open menus, relative to x y).

```json
{
  "properties": {},
  "type": "object"
}
```
