//! Single-instance toggle sockets.
//!
//! A launcher surface runs as a `--server` daemon that binds a Unix socket in
//! the runtime dir. Later invocations of the same command connect, write one
//! line, and exit — which is what makes "run the command again to close it"
//! work. If nothing is listening the caller falls back to a one-shot window.

use std::{
    env, fs,
    io::{self, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::Duration,
};

use directories::ProjectDirs;

/// How often the GTK main loop drains messages delivered by the listener thread.
const DRAIN_INTERVAL: Duration = Duration::from_millis(24);

/// A request that can travel over a toggle socket as a single line of text.
pub trait ToggleMessage: Send + Sized + 'static {
    fn serialize(&self) -> String;
    fn parse(text: &str) -> Option<Self>;
}

/// Unlinks the socket file when the server shuts down.
pub struct SocketFileGuard {
    path: PathBuf,
}

impl Drop for SocketFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn socket_path(file_name: &str, tmp_fallback: &str) -> Option<PathBuf> {
    let project_dirs = ProjectDirs::from("io.github", "ionix", "ioexplorer");
    project_dirs
        .as_ref()
        .and_then(|dirs| dirs.runtime_dir().map(|dir| dir.join(file_name)))
        .or_else(|| {
            project_dirs
                .as_ref()
                .and_then(|dirs| dirs.state_dir().map(|dir| dir.join(file_name)))
        })
        .or_else(|| Some(env::temp_dir().join(tmp_fallback)))
}

/// Binds the toggle socket, returning `Ok(None)` when a live server already owns it.
///
/// A socket file left behind by a crashed server is detected by probing it with
/// a connect, then removed so the new server can take over.
pub fn bind(
    file_name: &str,
    tmp_fallback: &str,
) -> io::Result<Option<(UnixListener, SocketFileGuard)>> {
    let path = socket_path(file_name, tmp_fallback)
        .ok_or_else(|| io::Error::other("missing socket path"))?;
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return Err(error);
    }

    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Ok(None);
        }

        let _ = fs::remove_file(&path);
    }

    let listener = UnixListener::bind(&path)?;
    Ok(Some((listener, SocketFileGuard { path })))
}

/// Sends one message to a running server, failing when none is listening.
pub fn send<M: ToggleMessage>(file_name: &str, tmp_fallback: &str, message: &M) -> io::Result<()> {
    let path = socket_path(file_name, tmp_fallback)
        .ok_or_else(|| io::Error::other("missing socket path"))?;
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(message.serialize().as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Accepts connections on a background thread, forwarding parsed messages.
pub fn spawn_listener<M: ToggleMessage>(listener: UnixListener) -> mpsc::Receiver<M> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = String::new();
                    match stream.read_to_string(&mut request) {
                        Ok(_) => {
                            if let Some(message) = M::parse(&request) {
                                let _ = sender.send(message);
                            }
                        }
                        Err(error) => tracing::warn!(%error, "failed to read toggle request"),
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "toggle listener stopped");
                    break;
                }
            }
        }
    });
    receiver
}

/// Drains the listener channel from the GTK main loop.
pub fn install_receiver<M: 'static>(
    receiver: mpsc::Receiver<M>,
    handler: impl Fn(M) + 'static,
) -> glib::SourceId {
    glib::timeout_add_local(DRAIN_INTERVAL, move || {
        while let Ok(message) = receiver.try_recv() {
            handler(message);
        }
        glib::ControlFlow::Continue
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct TestMessage {
        value: String,
    }

    impl ToggleMessage for TestMessage {
        fn serialize(&self) -> String {
            format!("test {}\n", self.value)
        }

        fn parse(text: &str) -> Option<Self> {
            let mut parts = text.split_whitespace();
            (parts.next()? == "test").then_some(())?;
            let value = parts.next()?.to_string();
            parts.next().is_none().then_some(Self { value })
        }
    }

    #[test]
    fn round_trips_a_message() {
        let message = TestMessage {
            value: "toggle".to_string(),
        };

        let parsed = TestMessage::parse(&message.serialize()).expect("parsable");

        assert_eq!(parsed, message);
    }

    #[test]
    fn rejects_malformed_messages() {
        assert!(TestMessage::parse("").is_none());
        assert!(TestMessage::parse("other value\n").is_none());
        assert!(TestMessage::parse("test one two\n").is_none());
    }

    #[test]
    fn socket_path_ends_with_the_requested_file_name() {
        let path = socket_path("spotlight.sock", "ioexplorer-spotlight.sock")
            .expect("a socket path is always resolvable");

        assert!(
            path.ends_with("spotlight.sock") || path.ends_with("ioexplorer-spotlight.sock"),
            "unexpected socket path: {}",
            path.display()
        );
    }
}
