use crate::gui;
use crate::tearoff;
use crate::thp;
use crate::vulkan;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// The exit statuses ui/Opener.qml reads. 0 is a successful handoff and needs no name.
pub const FAILED: i32 = 2;
pub const IS_DIRECTORY: i32 = 3;

// Canonical, so a file named --output=/etc/x cannot be read as a flag by the child.
fn resolved(path: &str) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

// gio open is the OEM route: it asks the desktop database, so Terminal=true is honoured; see AGENTS.md "Opening a file".
pub fn open(path: &str) -> i32 {
    open_all(&[path.to_string()])
}

pub fn open_all(paths: &[String]) -> i32 {
    if paths.is_empty() {
        return FAILED;
    }
    let mut targets = Vec::with_capacity(paths.len());
    for path in paths {
        match resolved(path) {
            Some(p) => targets.push(p),
            None => {
                // The reason is elided, never shown raw, and the path is the user's own input.
                eprintln!("flea: that file could not be opened, check that it still exists");
                return FAILED;
            }
        }
    }
    if targets.len() == 1 && targets[0].is_dir() {
        return IS_DIRECTORY;
    }
    if targets.iter().any(|t| t.is_dir()) {
        eprintln!("flea: directories cannot be opened alongside files");
        return FAILED;
    }
    // The setting is inherited across exec, so this is the last point that can hand it back.
    thp::enable();
    if targets.len() > 1 {
        if let Some(plan) = batch_plan(&targets) {
            let mut refused = false;
            for (desktop, files) in plan {
                let mut args: Vec<&Path> = Vec::with_capacity(files.len() + 1);
                args.push(&desktop);
                for file in &files {
                    args.push(file);
                }
                if run_gio("launch", &args) != 0 {
                    refused = true;
                }
            }
            if refused {
                eprintln!("flea: gio launch refused part of that selection, so some of it did not open");
                return FAILED;
            }
            return 0;
        }
    }
    let mut args: Vec<&Path> = Vec::with_capacity(targets.len());
    for target in &targets {
        args.push(target);
    }
    let status = run_gio("open", &args);
    match status {
        0 => 0,
        // A launcher that refused, which a spawn nobody waited on used to report as a clean handoff.
        _ => {
            eprintln!("flea: gio open refused that file, so no application on this system took it");
            FAILED
        }
    }
}

// gio open starts the handler once per file, so a selection of N opened N programs where
// Nautilus opened one. It groups the selection by default application in make_activation_parameters
// (src/nautilus-files-view.c) and launches each group once, so a player whose Exec line takes a
// URI list gets the whole selection as one playlist. The desktop database still decides the
// handler; Flea only asks which one through xdg-mime and hands gio the desktop file it named.
// Nothing is parsed out of the file. A single file keeps the gio open route, which is the one
// that honours Terminal=true; see AGENTS.md "Opening a file".
fn batch_plan(targets: &[PathBuf]) -> Option<Vec<(PathBuf, Vec<PathBuf>)>> {
    let db = crate::backend::mime::Db::load();
    // Grouped by the handler, not by the type: two types can share one application, and
    // Nautilus hashes on g_app_info_get_id for that reason. Grouping by type would start that
    // application twice where Nautilus starts it once.
    let mut groups: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    // One lookup per distinct type, not one per file: a hundred-row selection of a single
    // type is one subprocess here, where asking per file spent a hundred.
    let mut asked: Vec<(String, PathBuf)> = Vec::new();
    for target in targets {
        let mime = db.lookup(&target.to_string_lossy())?.to_string();
        // A type with no handler, or one whose desktop file is not installed, leaves the whole
        // batch on the gio open route rather than opening part of a selection.
        let desktop = match asked.iter().find(|(name, _)| *name == mime) {
            Some((_, path)) => path.clone(),
            None => {
                let path = desktop_file(&mime)?;
                asked.push((mime, path.clone()));
                path
            }
        };
        match groups.iter_mut().find(|(name, _)| *name == desktop) {
            Some((_, files)) => files.push(target.clone()),
            None => groups.push((desktop, vec![target.clone()])),
        }
    }
    Some(groups)
}

// The desktop file the database names for a type, or None when it names none this box holds.
// Asked through gio itself so the handoff stays the one program open.rs is allowed to name.
fn desktop_file(mime: &str) -> Option<PathBuf> {
    let asked = Command::new("gio").arg("mime").arg(mime).output().ok()?;
    // "Default application for "audio/mpeg": mpv.desktop" — the name is what follows the last colon.
    let line = String::from_utf8(asked.stdout).ok()?;
    let app = line.lines().next()?.rsplit_once(':')?.1.trim().to_string();
    if app.is_empty() {
        return None;
    }
    // Each variable carries its own default, per the XDG spec, so a box that exports neither still
    // finds a handler the user installed in ~/.local/share/applications.
    let mut dirs: Vec<PathBuf> = Vec::new();
    match std::env::var("XDG_DATA_HOME") {
        Ok(home) if !home.is_empty() => dirs.push(PathBuf::from(home)),
        _ => {
            if let Ok(home) = std::env::var("HOME") {
                dirs.push(PathBuf::from(home).join(".local/share"));
            }
        }
    }
    let system = match std::env::var("XDG_DATA_DIRS") {
        Ok(v) if !v.is_empty() => v,
        _ => "/usr/local/share:/usr/share".to_string(),
    };
    dirs.extend(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    dirs.iter().map(|d| d.join("applications").join(&app)).find(|p| p.is_file())
}

// Every route is one gio invocation, so the guards Flea hands back and the exit status live here.
fn run_gio(verb: &str, args: &[&Path]) -> i32 {
    let mut launcher = Command::new("gio");
    // The display-GPU pin is Qt's alone, and only this launcher's own pin is dropped.
    vulkan::drop_display_pin(&mut launcher);
    // The platform theme Flea traded for its own startup is Qt's alone too, and is handed back here.
    gui::restore_platform_theme(&mut launcher);
    // The tear-off hand-off belongs to the one window a tear-off starts, never to a program it opens.
    tearoff::drop_env(&mut launcher);
    launcher.arg(verb);
    launcher.args(args);
    // corner: waited for, not detached, and on an archive that wait is a cold handler start; see AGENTS.md "Opening a file".
    match launcher
        // The handler outlives us, so an inherited pipe would kill it on its first write; see AGENTS.md "Opening a file".
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group, so nothing that later kills Flea's group reaches the opened program.
        .process_group(0)
        .status()
    {
        Ok(status) if status.success() => 0,
        Ok(_) => FAILED,
        Err(_) => {
            eprintln!("flea: nothing on this system could be asked to open that file");
            FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_all_rejects_empty_paths() {
        assert_eq!(open_all(&[]), FAILED);
    }

    #[test]
    fn open_all_rejects_nonexistent_file() {
        assert_eq!(open_all(&["/nonexistent_file_path_xyz123".to_string()]), FAILED);
    }

    #[test]
    fn open_all_single_directory_returns_is_directory() {
        assert_eq!(open_all(&["/".to_string()]), IS_DIRECTORY);
    }

    #[test]
    fn open_all_multiple_with_directory_fails() {
        assert_eq!(open_all(&["/".to_string(), "/etc".to_string()]), FAILED);
    }
}
