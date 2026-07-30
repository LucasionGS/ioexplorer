use std::{env, path::PathBuf, process::Command};

use gio::{glib, prelude::*};
use tracing_subscriber::{EnvFilter, fmt};

use crate::selector;

const PORTAL_BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.ioexplorer";
const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const FILE_CHOOSER_INTERFACE: &str = "org.freedesktop.impl.portal.FileChooser";
const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_ERROR: u32 = 2;

const INTROSPECTION_XML: &str = r#"
<node>
  <interface name='org.freedesktop.impl.portal.FileChooser'>
    <method name='OpenFile'>
      <arg type='o' name='handle' direction='in'/>
      <arg type='s' name='app_id' direction='in'/>
      <arg type='s' name='parent_window' direction='in'/>
      <arg type='s' name='title' direction='in'/>
      <arg type='a{sv}' name='options' direction='in'/>
      <arg type='u' name='response' direction='out'/>
      <arg type='a{sv}' name='results' direction='out'/>
    </method>
    <method name='SaveFile'>
      <arg type='o' name='handle' direction='in'/>
      <arg type='s' name='app_id' direction='in'/>
      <arg type='s' name='parent_window' direction='in'/>
      <arg type='s' name='title' direction='in'/>
      <arg type='a{sv}' name='options' direction='in'/>
      <arg type='u' name='response' direction='out'/>
      <arg type='a{sv}' name='results' direction='out'/>
    </method>
    <method name='SaveFiles'>
      <arg type='o' name='handle' direction='in'/>
      <arg type='s' name='app_id' direction='in'/>
      <arg type='s' name='parent_window' direction='in'/>
      <arg type='s' name='title' direction='in'/>
      <arg type='a{sv}' name='options' direction='in'/>
      <arg type='u' name='response' direction='out'/>
      <arg type='a{sv}' name='results' direction='out'/>
    </method>
  </interface>
</node>
"#;

pub fn run() -> glib::ExitCode {
    init_logging();

    let main_loop = glib::MainLoop::new(None, false);
    let quit_loop = main_loop.clone();
    let owner_id = gio::bus_own_name(
        gio::BusType::Session,
        PORTAL_BUS_NAME,
        gio::BusNameOwnerFlags::NONE,
        move |connection, _name| {
            if let Err(error) = register_file_chooser(&connection) {
                tracing::error!(%error, "failed to register IoExplorer portal backend");
            }
        },
        |_connection, name| tracing::info!(%name, "IoExplorer portal backend registered"),
        move |connection, name| {
            tracing::error!(%name, has_connection = connection.is_some(), "lost IoExplorer portal backend bus name");
            quit_loop.quit();
        },
    );

    main_loop.run();
    gio::bus_unown_name(owner_id);
    glib::ExitCode::SUCCESS
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}

fn register_file_chooser(connection: &gio::DBusConnection) -> Result<(), glib::Error> {
    let node = gio::DBusNodeInfo::for_xml(INTROSPECTION_XML)?;
    let interface = node.lookup_interface(FILE_CHOOSER_INTERFACE).unwrap();
    connection
        .register_object(PORTAL_OBJECT_PATH, &interface)
        .method_call(
            |_connection,
             _sender,
             _object_path,
             _interface_name,
             method_name,
             parameters,
             invocation| {
                let (response, results) = handle_file_chooser_call(method_name, &parameters);
                invocation.return_value(Some(&response_variant(response, results)));
            },
        )
        .build()?;
    Ok(())
}

fn handle_file_chooser_call(method_name: &str, parameters: &glib::Variant) -> (u32, glib::Variant) {
    let title = parameters.child_get::<String>(3);
    let options_variant = parameters.child_value(4);
    let options = glib::VariantDict::new(Some(&options_variant));
    let mut args = vec!["--chooser".to_string()];

    match method_name {
        "OpenFile" => {
            args.extend(["--chooser-mode".to_string(), "open".to_string()]);
            if lookup_bool(&options, "multiple") {
                args.push("--multiple".to_string());
            }
            if lookup_bool(&options, "directory") {
                args.push("--directory".to_string());
            }
        }
        "SaveFile" => {
            args.extend(["--chooser-mode".to_string(), "save".to_string()]);
            if let Some(name) = lookup_string(&options, "current_name") {
                args.extend(["--current-name".to_string(), name]);
            }
            if let Some(path) = lookup_path_bytes(&options, "current_file") {
                args.extend([
                    "--current-file".to_string(),
                    path.to_string_lossy().to_string(),
                ]);
            }
        }
        "SaveFiles" => {
            args.extend(["--chooser-mode".to_string(), "save-files".to_string()]);
            for name in lookup_path_byte_array(&options, "files") {
                args.extend(["--file-name".to_string(), name]);
            }
        }
        _ => return (RESPONSE_ERROR, empty_results()),
    }

    args.extend(["--title".to_string(), title]);
    if let Some(label) = lookup_string(&options, "accept_label") {
        args.extend(["--accept-label".to_string(), label]);
    }
    if let Some(path) = lookup_path_bytes(&options, "current_folder") {
        args.extend([
            "--current-folder".to_string(),
            path.to_string_lossy().to_string(),
        ]);
    }

    run_selector(args)
}

fn run_selector(args: Vec<String>) -> (u32, glib::Variant) {
    let mut command = Command::new(selector_binary());
    command.args(args);
    for (key, value) in session_display_environment() {
        command.env(key, value);
    }

    let output = match command.output() {
        Ok(output) => output,
        Err(error) => {
            tracing::error!(%error, "failed to launch IoExplorer selector");
            return (RESPONSE_ERROR, empty_results());
        }
    };

    if output.status.code() == Some(i32::from(selector::EXIT_CANCELLED)) {
        return (RESPONSE_CANCELLED, empty_results());
    }

    if !output.status.success() {
        // Reporting this as a cancellation is what makes a broken chooser look
        // like nothing happening at all, so say what went wrong instead.
        tracing::error!(
            status = ?output.status.code(),
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "IoExplorer selector exited without choosing"
        );
        return (RESPONSE_ERROR, empty_results());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let uris = stdout
        .lines()
        .filter(|line| line.starts_with("file://"))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if uris.is_empty() {
        return (RESPONSE_CANCELLED, empty_results());
    }

    let results = glib::VariantDict::new(None);
    results.insert_value("uris", &uris.to_variant());
    (RESPONSE_SUCCESS, results.to_variant())
}

/// Variables the chooser needs in order to open a window at all.
const SESSION_DISPLAY_KEYS: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
];

/// The session's display variables, taken from systemd rather than trusted from
/// our own environment.
///
/// D-Bus can activate this backend before the compositor has published
/// `WAYLAND_DISPLAY` into the activation environment — in which case we inherit
/// no display at all, and every chooser we spawn dies with "Failed to open
/// display" before drawing anything. The session manager always knows the
/// current values, so ask it rather than assume we were started late enough.
///
/// Only fills gaps: a variable we already hold was set deliberately and wins.
fn session_display_environment() -> Vec<(String, String)> {
    let missing = SESSION_DISPLAY_KEYS
        .iter()
        .filter(|key| env::var_os(key).is_none())
        .count();
    if missing == 0 {
        return Vec::new();
    }

    let output = match Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
    {
        Ok(output) if output.status.success() => output.stdout,
        Ok(_) | Err(_) => {
            tracing::warn!("could not read the session environment from systemd");
            return Vec::new();
        }
    };

    let imported = parse_systemd_environment(&String::from_utf8_lossy(&output))
        .into_iter()
        .filter(|(key, _)| env::var_os(key).is_none())
        .collect::<Vec<_>>();
    if !imported.is_empty() {
        tracing::info!(
            keys = ?imported.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            "took display variables from the session environment"
        );
    }
    imported
}

/// Parses `systemctl --user show-environment` output, keeping the display
/// variables and discarding everything else in the session environment.
fn parse_systemd_environment(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| SESSION_DISPLAY_KEYS.contains(key))
        .filter_map(|(key, value)| {
            let value = unquote_systemd_value(value)?;
            (!value.is_empty()).then(|| (key.to_string(), value))
        })
        .collect()
}

/// systemd quotes values that need it. Display variables never do, so anything
/// carrying escapes is skipped rather than half-parsed into something wrong.
fn unquote_systemd_value(value: &str) -> Option<String> {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return (!value.contains(['"', '\'', '\\'])).then(|| value.to_string());
    };

    (!inner.contains('\\')).then(|| inner.to_string())
}

fn selector_binary() -> PathBuf {
    if let Some(path) = env::var_os("IOEXPLORER_SELECTOR") {
        return PathBuf::from(path);
    }

    if let Ok(current_exe) = env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        let sibling = parent.join("ioexplorer");
        if sibling.exists() {
            return sibling;
        }
    }

    PathBuf::from("ioexplorer")
}

fn lookup_bool(options: &glib::VariantDict, key: &str) -> bool {
    options.lookup::<bool>(key).ok().flatten().unwrap_or(false)
}

fn lookup_string(options: &glib::VariantDict, key: &str) -> Option<String> {
    options.lookup::<String>(key).ok().flatten()
}

fn lookup_path_bytes(options: &glib::VariantDict, key: &str) -> Option<PathBuf> {
    let bytes = options.lookup::<Vec<u8>>(key).ok().flatten()?;
    bytes_to_path(bytes)
}

fn lookup_path_byte_array(options: &glib::VariantDict, key: &str) -> Vec<String> {
    options
        .lookup::<Vec<Vec<u8>>>(key)
        .ok()
        .flatten()
        .unwrap_or_default()
        .into_iter()
        .filter_map(bytes_to_string)
        .collect()
}

fn bytes_to_path(bytes: Vec<u8>) -> Option<PathBuf> {
    bytes_to_string(bytes).map(PathBuf::from)
}

fn bytes_to_string(mut bytes: Vec<u8>) -> Option<String> {
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .ok()
        .filter(|value| !value.is_empty())
}

fn empty_results() -> glib::Variant {
    glib::VariantDict::new(None).to_variant()
}

fn response_variant(response: u32, results: glib::Variant) -> glib::Variant {
    glib::Variant::tuple_from_iter([response.to_variant(), results])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_null_terminated_bytes_to_path() {
        assert_eq!(
            bytes_to_path(b"/tmp/example\0".to_vec()),
            Some(PathBuf::from("/tmp/example"))
        );
    }

    #[test]
    fn rejects_empty_byte_paths() {
        assert_eq!(bytes_to_path(b"\0".to_vec()), None);
    }

    #[test]
    fn reads_options_dict_from_method_parameters() {
        let options = glib::VariantDict::new(None);
        options.insert_value("multiple", &true.to_variant());

        let parameters = glib::Variant::tuple_from_iter([
            glib::variant::ObjectPath::try_from("/org/freedesktop/portal/desktop/request/test")
                .unwrap()
                .to_variant(),
            "app.id".to_variant(),
            "".to_variant(),
            "Open File".to_variant(),
            options.to_variant(),
        ]);

        let options_variant = parameters.child_value(4);
        let options = glib::VariantDict::new(Some(&options_variant));

        assert!(lookup_bool(&options, "multiple"));
    }

    #[test]
    fn keeps_only_display_variables_from_the_session_environment() {
        let imported = parse_systemd_environment(
            "LANG=en_US.UTF-8\n\
             WAYLAND_DISPLAY=wayland-1\n\
             PATH=/usr/bin\n\
             DISPLAY=:0\n",
        );

        assert_eq!(
            imported,
            vec![
                ("WAYLAND_DISPLAY".to_string(), "wayland-1".to_string()),
                ("DISPLAY".to_string(), ":0".to_string()),
            ]
        );
    }

    #[test]
    fn unwraps_quoted_values_and_skips_escaped_ones() {
        assert_eq!(
            parse_systemd_environment("XDG_CURRENT_DESKTOP=\"GNOME:Ubuntu\"\n"),
            vec![(
                "XDG_CURRENT_DESKTOP".to_string(),
                "GNOME:Ubuntu".to_string()
            )]
        );
        // Half-parsing an escaped value would hand the chooser a wrong display.
        assert!(parse_systemd_environment("DISPLAY=\"a\\nb\"\n").is_empty());
        assert!(parse_systemd_environment("DISPLAY=\n").is_empty());
    }

    #[test]
    fn response_variant_matches_dbus_signature() {
        let response = response_variant(RESPONSE_CANCELLED, empty_results());

        assert_eq!(response.type_().as_str(), "(ua{sv})");
    }
}
