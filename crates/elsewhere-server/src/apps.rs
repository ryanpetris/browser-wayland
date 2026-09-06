//! The application menu: launchers from the `.desktop` files of the XDG data directories, and their
//! icons, for the viewer's menu and the API.

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use schemars::JsonSchema;
use serde::Serialize;

/// One launcher, as the menu shows it.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AppInfo {
    /// The desktop file's name without `.desktop`; what `launch` takes.
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// The entry's categories (`Network`, `Office`, ...), for grouping.
    pub categories: Vec<String>,
}

struct Entry {
    name: String,
    comment: Option<String>,
    icon: Option<String>,
    categories: Vec<String>,
    exec: String,
}

/// `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`, with the spec's defaults (an empty variable counts as
/// unset); the first that has a file wins.
fn data_dirs() -> Vec<PathBuf> {
    let set = |name: &str| env::var(name).ok().filter(|v| !v.is_empty());
    let home = set("XDG_DATA_HOME").map(PathBuf::from).or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = set("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter().chain(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from)).collect()
}

/// The `[Desktop Entry]` group of a file, if it is an application a menu would show.
fn parse(text: &str) -> Option<Entry> {
    let mut kv = HashMap::new();
    let mut in_entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && let Some((k, v)) = line.split_once('=') {
            kv.entry(k.trim()).or_insert(v.trim());
        }
    }
    // an application, not hidden, not meant for one particular desktop, not one that needs a terminal
    if kv.get("Type") != Some(&"Application")
        || kv.get("NoDisplay") == Some(&"true")
        || kv.get("Hidden") == Some(&"true")
        || kv.contains_key("OnlyShowIn")
        || kv.get("Terminal") == Some(&"true")
        || kv.get("TryExec").is_some_and(|bin| !installed(bin))
    {
        return None;
    }
    let list = |key: &str| kv.get(key).map(|v| v.split(';').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default();
    Some(Entry {
        name: kv.get("Name")?.to_string(),
        comment: kv.get("Comment").map(|s| s.to_string()),
        icon: kv.get("Icon").map(|s| s.to_string()),
        categories: list("Categories"),
        exec: command(kv.get("Exec")?),
    })
}

fn installed(bin: &str) -> bool {
    let executable = |p: &Path| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if bin.contains('/') {
        return executable(Path::new(bin));
    }
    env::var_os("PATH").is_some_and(|p| env::split_paths(&p).any(|d| executable(&d.join(bin))))
}

/// The Exec line without its field codes (`%u`, `%F`, ...: there is nothing to open).
fn command(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => {
                if chars.next() == Some('%') {
                    out.push('%');
                }
            }
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Every application in the data directories, by name.
pub fn list() -> Vec<AppInfo> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in data_dirs() {
        let Ok(files) = fs::read_dir(dir.join("applications")) else { continue };
        for path in files.flatten().map(|e| e.path()) {
            let Some(id) = path.file_name().and_then(|f| f.to_str()).and_then(|f| f.strip_suffix(".desktop")) else { continue };
            // the first directory's file counts, hidden or not: that is how a user hides a system entry
            if !seen.insert(id.to_string()) {
                continue;
            }
            if let Some(e) = fs::read_to_string(&path).ok().and_then(|t| parse(&t)) {
                apps.push(AppInfo { id: id.to_string(), name: e.name, comment: e.comment, categories: e.categories });
            }
        }
    }
    apps.sort_by_cached_key(|a| a.name.to_lowercase());
    apps
}

fn entry(id: &str) -> Option<Entry> {
    if id.contains('/') {
        return None; // an id names a file in the applications directories, nothing beyond them
    }
    let file = format!("{id}.desktop");
    let path = data_dirs().into_iter().map(|d| d.join("applications").join(&file)).find(|p| p.is_file())?;
    parse(&fs::read_to_string(path).ok()?)
}

/// The command line that starts an application.
/// A window's application by its app id, for a message: the launcher's name when a desktop file has
/// that id (`thunar` → "Thunar File Manager", `org.gnome.Nautilus` → "Files"), else the id's last part.
pub fn display_name(app_id: &str) -> String {
    list().into_iter().find(|a| a.id == app_id).map(|a| a.name).unwrap_or_else(|| app_id.rsplit('.').next().unwrap_or(app_id).to_string())
}

pub fn exec(id: &str) -> Option<String> {
    entry(id).map(|e| e.exec)
}

const SIZES: [&str; 10] = ["scalable", "512x512", "256x256", "128x128", "96x96", "64x64", "48x48", "32x32", "24x24", "16x16"];

/// An application's icon: the file and its media type.
pub fn icon(id: &str) -> Option<(PathBuf, &'static str)> {
    icon_file(&entry(id)?.icon?)
}

/// An icon a client named through xdg-toplevel-icon: a stock name, never a path.
pub fn named_icon(name: &str) -> Option<(PathBuf, &'static str)> {
    (!name.contains('/')).then(|| icon_file(name)).flatten()
}

/// A window's launcher's icon (the app id is the desktop id for Wayland clients; X11 classes are
/// often the lowercase name).
pub fn launcher_icon(app_id: &str) -> Option<(PathBuf, &'static str)> {
    entry(app_id).or_else(|| entry(&app_id.to_lowercase()))?.icon.and_then(|i| icon_file(&i))
}

/// An icon name (or path) as a file and its media type: SVG or PNG from the icon themes (hicolor,
/// where applications install their own, then the others) or the pixmaps directory.
pub(crate) fn icon_file(icon: &str) -> Option<(PathBuf, &'static str)> {
    let mime = |p: &Path| match p.extension().and_then(|e| e.to_str()) {
        Some("svg") => Some("image/svg+xml"),
        Some("png") => Some("image/png"),
        _ => None,
    };
    if icon.starts_with('/') {
        let p = PathBuf::from(icon);
        return mime(&p).filter(|_| p.is_file()).map(|m| (p, m));
    }
    let name = icon.trim_end_matches(".png").trim_end_matches(".svg").trim_end_matches(".xpm");
    for data in data_dirs() {
        let icons = data.join("icons");
        let mut themes: Vec<String> = fs::read_dir(&icons).ok().into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|t| t != "hicolor").collect();
        themes.sort();
        for theme in std::iter::once("hicolor".to_string()).chain(themes) {
            for size in SIZES {
                for ext in ["svg", "png"] {
                    let p = icons.join(&theme).join(size).join("apps").join(format!("{name}.{ext}"));
                    if p.is_file() {
                        return mime(&p).map(|m| (p, m));
                    }
                }
            }
        }
        for ext in ["svg", "png"] {
            let p = data.join("pixmaps").join(format!("{name}.{ext}"));
            if p.is_file() {
                return mime(&p).map(|m| (p, m));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn exec_loses_its_field_codes() {
        assert_eq!(super::command("/usr/lib/firefox/firefox %u"), "/usr/lib/firefox/firefox");
        assert_eq!(super::command("gimp-3.0 %U"), "gimp-3.0");
        assert_eq!(super::command("env FOO=100%% app --file %f %i"), "env FOO=100% app --file");
    }

    #[test]
    fn hidden_and_terminal_entries_are_not_shown() {
        let base = "[Desktop Entry]\nType=Application\nName=A\nExec=a\n";
        assert!(super::parse(base).is_some());
        assert!(super::parse(&format!("{base}NoDisplay=true\n")).is_none());
        assert!(super::parse(&format!("{base}Terminal=true\n")).is_none());
        assert!(super::parse(&format!("{base}OnlyShowIn=GNOME;\n")).is_none());
        assert!(super::parse("[Desktop Entry]\nType=Link\nName=A\nURL=x\n").is_none());
        assert!(super::parse("[Desktop Action new]\nType=Application\nName=A\nExec=a\n").is_none());
    }
}
