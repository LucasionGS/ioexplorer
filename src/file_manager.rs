use std::{env, path::PathBuf, process::Command};

use gio::glib;
use tracing_subscriber::{EnvFilter, fmt};
use url::Url;

const FILE_MANAGER_BUS_NAME: &str = "org.freedesktop.FileManager1";
const FILE_MANAGER_OBJECT_PATH: &str = "/org/freedesktop/FileManager1";
const FILE_MANAGER_INTERFACE: &str = "org.freedesktop.FileManager1";

const INTROSPECTION_XML: &str = r#"
<node>
  <interface name='org.freedesktop.FileManager1'>
    <method name='ShowItems'>
      <arg type='as' name='URIs' direction='in'/>
      <arg type='s' name='StartupId' direction='in'/>
    </method>
    <method name='ShowFolders'>
      <arg type='as' name='URIs' direction='in'/>
      <arg type='s' name='StartupId' direction='in'/>
    </method>
    <method name='ShowItemProperties'>
      <arg type='as' name='URIs' direction='in'/>
      <arg type='s' name='StartupId' direction='in'/>
    </method>
  </interface>
</node>
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaunchMode {
    OpenFolders,
    SelectItems,
}

pub fn run() -> glib::ExitCode {
    init_logging();

    let main_loop = glib::MainLoop::new(None, false);
    let quit_loop = main_loop.clone();
    let owner_id = gio::bus_own_name(
        gio::BusType::Session,
        FILE_MANAGER_BUS_NAME,
        gio::BusNameOwnerFlags::NONE,
        move |connection, _name| {
            if let Err(error) = register_file_manager(&connection) {
                tracing::error!(%error, "failed to register IoExplorer FileManager1 service");
            }
        },
        |_connection, name| tracing::info!(%name, "IoExplorer FileManager1 service registered"),
        move |connection, name| {
            tracing::error!(%name, has_connection = connection.is_some(), "lost FileManager1 bus name");
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

fn register_file_manager(connection: &gio::DBusConnection) -> Result<(), glib::Error> {
    let node = gio::DBusNodeInfo::for_xml(INTROSPECTION_XML)?;
    let interface = node.lookup_interface(FILE_MANAGER_INTERFACE).unwrap();
    connection
        .register_object(FILE_MANAGER_OBJECT_PATH, &interface)
        .method_call(
            |_connection,
             _sender,
             _object_path,
             _interface_name,
             method_name,
             parameters,
             invocation| {
                handle_file_manager_call(method_name, &parameters);
                invocation.return_value(None);
            },
        )
        .build()?;
    Ok(())
}

fn handle_file_manager_call(method_name: &str, parameters: &glib::Variant) {
    let uris = parameters
        .try_child_get::<Vec<String>>(0)
        .ok()
        .flatten()
        .unwrap_or_default();
    let paths = paths_from_uris(&uris);

    match method_name {
        "ShowFolders" => launch_ioexplorer(folders_from_paths(paths), LaunchMode::OpenFolders),
        "ShowItems" | "ShowItemProperties" => launch_ioexplorer(paths, LaunchMode::SelectItems),
        unknown => tracing::warn!(%unknown, "unknown FileManager1 method"),
    }
}

fn launch_ioexplorer(paths: Vec<PathBuf>, mode: LaunchMode) {
    let mut command = Command::new(app_binary());
    if mode == LaunchMode::SelectItems {
        command.arg("--select");
    }
    command.args(paths);

    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => {
            tracing::error!(%error, "failed to launch IoExplorer from FileManager1 service");
        }
    }
}

fn folders_from_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter_map(|path| {
            if path.is_dir() {
                Some(path)
            } else {
                path.parent().map(ToOwned::to_owned)
            }
        })
        .collect()
}

fn paths_from_uris(uris: &[String]) -> Vec<PathBuf> {
    uris.iter().filter_map(|uri| path_from_uri(uri)).collect()
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

fn app_binary() -> PathBuf {
    if let Some(path) = env::var_os("IOEXPLORER_APP") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_uris_to_paths() {
        assert_eq!(
            path_from_uri("file:///tmp/IoExplorer%20Test"),
            Some(PathBuf::from("/tmp/IoExplorer Test"))
        );
    }

    #[test]
    fn ignores_non_file_uris() {
        assert_eq!(path_from_uri("https://example.com/file"), None);
    }
}
