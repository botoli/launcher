use std::{rc::Rc, time::Duration};

use launcher_core::ping_minecraft_server;
use slint::ComponentHandle;
use tokio::{runtime::Handle, sync::watch};

use crate::AppWindow;

const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServerView {
    Unconfigured,
    Checking,
    Online { ping_ms: i32 },
    Unavailable,
}

impl ServerView {
    fn signal(&self) -> i32 {
        match self {
            Self::Online { ping_ms: 0..=100 } => 3,
            Self::Online { ping_ms: 101..=250 } => 2,
            Self::Online { .. } => 1,
            _ => 0,
        }
    }
}

pub(crate) fn apply_server_view(window: &AppWindow, view: &ServerView) {
    let (label, online, ping) = match view {
        ServerView::Unconfigured => ("Сервер не задан", false, -1),
        ServerView::Checking => ("Проверка сервера", false, -1),
        ServerView::Online { ping_ms } => ("Сервер онлайн", true, *ping_ms),
        ServerView::Unavailable => ("Нет ответа сервера", false, -1),
    };
    window.set_server_status(label.into());
    window.set_server_online(online);
    window.set_server_signal(view.signal());
    window.set_ping_ms(ping);
}

#[derive(Clone, Debug)]
struct Observation {
    address: String,
    view: ServerView,
}

pub(crate) struct ServerMonitor {
    target: watch::Sender<String>,
}

impl ServerMonitor {
    pub fn start(window: &AppWindow, runtime: &Handle, address: &str) -> Rc<Self> {
        let (target, targets) = watch::channel(address.to_owned());
        let (updates, mut observations) = watch::channel::<Option<Observation>>(None);
        let controller = Rc::new(Self { target });
        apply_server_view(window, &initial_view(address));
        let weak = window.as_weak();
        let current = controller.clone();
        slint::spawn_local(async move {
            while observations.changed().await.is_ok() {
                let Some(observation) = observations.borrow_and_update().clone() else {
                    continue;
                };
                let Some(window) = weak.upgrade() else {
                    break;
                };
                // An old probe must never restore the previous server's state.
                if *current.target.borrow() == observation.address
                    && window.get_server_address().as_str() == observation.address
                {
                    apply_server_view(&window, &observation.view);
                }
            }
        })
        .expect("UI event loop is available");
        runtime.spawn(monitor(targets, updates, REFRESH_INTERVAL, PROBE_TIMEOUT));
        controller
    }

    pub fn observe_address(&self, window: &AppWindow, address: Option<&str>) {
        let address = address.unwrap_or("");
        if *self.target.borrow() == address {
            return;
        }
        self.target.send_replace(address.to_owned());
        apply_server_view(window, &initial_view(address));
    }
}

fn initial_view(address: &str) -> ServerView {
    if address.is_empty() {
        ServerView::Unconfigured
    } else {
        ServerView::Checking
    }
}

async fn monitor(
    mut targets: watch::Receiver<String>,
    updates: watch::Sender<Option<Observation>>,
    refresh: Duration,
    timeout: Duration,
) {
    loop {
        let address = targets.borrow_and_update().clone();
        if address.is_empty() {
            tokio::select! {
                _ = updates.closed() => break,
                changed = targets.changed() => if changed.is_err() { break; },
            }
            continue;
        }
        let result = tokio::select! {
            biased;
            _ = updates.closed() => break,
            changed = targets.changed() => { if changed.is_err() { break; } continue; },
            result = ping_minecraft_server(&address, timeout) => result,
        };
        let view = match result {
            Ok(latency) => ServerView::Online {
                // Round up so a real sub-millisecond round trip is shown as 1 ms.
                ping_ms: latency
                    .as_micros()
                    .div_ceil(1000)
                    .clamp(1, i32::MAX as u128) as i32,
            },
            Err(error) => {
                tracing::debug!(server = %address, %error, "Minecraft server status unavailable");
                ServerView::Unavailable
            }
        };
        if updates.send(Some(Observation { address, view })).is_err() {
            break;
        }
        tokio::select! {
            _ = updates.closed() => break,
            changed = targets.changed() => if changed.is_err() { break; },
            _ = tokio::time::sleep(refresh) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_boundaries_never_report_an_unknown_ping_as_online() {
        for (ping_ms, signal) in [(1, 3), (100, 3), (101, 2), (250, 2), (251, 1), (900, 1)] {
            assert_eq!(ServerView::Online { ping_ms }.signal(), signal);
        }
        assert_eq!(ServerView::Checking.signal(), 0);
        assert_eq!(ServerView::Unavailable.signal(), 0);
        assert_eq!(initial_view(""), ServerView::Unconfigured);
    }

    #[tokio::test]
    async fn changing_address_cancels_an_inflight_probe_and_drops_the_old_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (target, targets) = watch::channel(address.clone());
        let (updates, mut observations) = watch::channel(None);
        let task = tokio::spawn(monitor(
            targets,
            updates,
            Duration::from_secs(60),
            Duration::from_secs(60),
        ));
        let (mut stream, _) = listener.accept().await.unwrap();
        target.send_replace("invalid/address".to_owned());
        tokio::time::timeout(Duration::from_secs(2), observations.changed())
            .await
            .unwrap()
            .unwrap();
        let result = observations.borrow_and_update().clone().unwrap();
        assert_eq!(result.address, "invalid/address");
        assert_eq!(result.view, ServerView::Unavailable);
        // The pending TCP connection was closed, not left running for 60 seconds.
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        drop(observations);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn repeats_probes_and_recovers_after_an_invalid_pong() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            for invalid_pong in [true, false] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let length = stream.read_u8().await.unwrap() as usize;
                let mut handshake = vec![0; length + 2];
                stream.read_exact(&mut handshake).await.unwrap();
                let json = br#"{"version":{"protocol":767}}"#;
                let mut response = vec![(json.len() + 2) as u8, 0, json.len() as u8];
                response.extend_from_slice(json);
                stream.write_all(&response).await.unwrap();
                let mut ping = [0; 10];
                stream.read_exact(&mut ping).await.unwrap();
                tokio::time::sleep(Duration::from_millis(30)).await;
                if invalid_pong {
                    ping[9] ^= 1;
                }
                stream.write_all(&ping).await.unwrap();
            }
        });
        let (_target, targets) = watch::channel(address);
        let (updates, mut observations) = watch::channel(None);
        let task = tokio::spawn(monitor(
            targets,
            updates,
            Duration::from_millis(20),
            Duration::from_secs(1),
        ));
        tokio::time::timeout(Duration::from_secs(2), observations.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            observations.borrow_and_update().as_ref().unwrap().view,
            ServerView::Unavailable
        );
        tokio::time::timeout(Duration::from_secs(2), observations.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            observations.borrow_and_update().as_ref().unwrap().view,
            ServerView::Online { ping_ms: 25.. }
        ));
        drop(observations);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }
}
