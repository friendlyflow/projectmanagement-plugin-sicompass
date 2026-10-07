//! Cloud sync of the board, through Sicompass Cloud.
//!
//! Off until the user ticks "enable cloud sync" in the board's settings. The
//! service itself (the switch, a sync a while after the last change, every
//! minute and on demand, as background tasks, and the merge of what another
//! computer changed) is `sicompass_sync::cloud`, the same one a third party's
//! plugin would use. What is here is what only the board knows: which
//! service, the row above the columns, and the host: the app, through the
//! plugin kit, plus threads of this process.
//!
//! - **Where the user stands** comes from the app (`license::standing` for
//!   Sicompass Cloud), which verified the certificate. The row says it, in the
//!   user's language, and never links anywhere: buying and redeeming are in
//!   store, tiers.
//! - **A sync** is a task, on a thread of its own: it reads the board's
//!   folder ([`storage_dir`]), gets the redeem token from `license::token`
//!   (which the app gives only for this plugin's own service) and talks to the
//!   server with [`net_send`]. Its result reaches the plugin through
//!   [`CloudHost::finished`], which `poll` drains. What it merged is written
//!   back on the plugin's thread, and the board is read again.
//!
//! The paywall is on the service, never on the data: whatever the standing,
//! the board is shown and saved to disk. Only the copy on the server is paid.

use std::cell::Cell;
use std::sync::mpsc;
use std::time::Duration;

use sicompass_sdk::ffon::FfonElement;
use sicompass_sdk::plugin::{TierStatus, host, license, storage_dir};
use sicompass_sdk::tags;
use sicompass_sync::cloud::Service;
pub use sicompass_sync::cloud::{Cloud, Finished, Host, TASK_SYNC};
use sicompass_sync::protocol::{Request, Response};
use sicompass_sync::row::Standing;

use crate::localize;

/// Sicompass Cloud, for the board. `plugin.json` names the same tier as its
/// `service` and allows only this server. The store is called "kanban" on the
/// server because that is what it holds, and existing copies are under it.
pub const SERVICE: Service = Service {
    tier: "friendlyflow/cloud",
    server: "https://store.sicompass.org",
    store: "kanban",
    enable_key: ENABLE_KEY,
    prefix: "projectmanagement",
};

/// The settings key of the "enable cloud sync" switch. It kept its name from
/// when this was a backup, so the switch survives the update.
pub const ENABLE_KEY: &str = "kanbanCloudBackup";

/// The `<id>` the sync row carries, so it is never taken for a column
/// (column and card ids are numbers).
pub const ROW_ID: &str = "cloud";

/// The host, plus what only the tasks need: the redeem token, and their
/// results.
pub trait CloudHost: Host {
    /// The redeem token for [`SERVICE`]'s tier, which the app gives only
    /// because `plugin.json` names that tier as this plugin's service.
    fn token(&self) -> Option<String>;

    /// The tasks [`Host::spawn`] started that have ended since the last call.
    fn finished(&self) -> Vec<TaskResult> {
        Vec::new()
    }
}

/// The row above the columns, while the switch is on.
pub fn row(cloud: &Cloud, host: &dyn Host) -> Option<FfonElement> {
    let text = cloud.row_text(host)?;
    Some(FfonElement::new_str(format!(
        "{}{text}",
        tags::format_id(ROW_ID)
    )))
}

/// Whether a row is the sync row (rendered, never stored).
pub fn is_row(raw: &str) -> bool {
    let prefix = raw.split("<input>").next().unwrap_or(raw);
    tags::extract_id(prefix).as_deref() == Some(ROW_ID)
}

/// Translate one of the service's messages from this plugin's locales.
pub fn translate(id: &str, args: &[(&str, String)]) -> String {
    let mut a = localize::Args::new();
    for (name, value) in args {
        a.set(name, value);
    }
    localize::t_args(id, &a)
}

/// The sync's HTTP: `send` for [`sicompass_sync::protocol`], over a
/// blocking client with rustls. Every status comes back as it is, because the
/// protocol reads the server's refusals (a 409 is "another computer synced first").
pub fn net_send(req: &Request) -> Result<Response, String> {
    use std::sync::OnceLock;
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(HTTP_TIMEOUT))
            .build()
            .into()
    });

    let mut builder = ureq::http::Request::builder()
        .method(req.method)
        .uri(&req.url);
    for (name, value) in &req.headers {
        builder = builder.header(name, value);
    }
    let result = match &req.body {
        Some(body) => agent.run(builder.body(body.as_slice()).map_err(|e| e.to_string())?),
        None => agent.run(builder.body(()).map_err(|e| e.to_string())?),
    };
    let mut resp = result.map_err(|e| format!("{}: {e}", req.url))?;
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|e| format!("{}: {e}", req.url))?;
    Ok(Response { status, body })
}

/// How long one request may take, start to end. An upload is a few MB at most.
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);

/// The most a reply may hold: a downloaded snapshot, with room for its encoding.
const MAX_RESPONSE_BYTES: u64 = 4 * sicompass_sync::snapshot::MAX_SNAPSHOT_BYTES as u64;

/// What a background task reported: its id and its outcome.
pub type TaskResult = (u64, Result<Vec<u8>, String>);

/// The host: the app, through the plugin kit, and threads of this process for
/// the syncs.
pub struct PluginHost {
    next_task: Cell<u64>,
    done_tx: mpsc::Sender<TaskResult>,
    done_rx: mpsc::Receiver<TaskResult>,
}

impl PluginHost {
    pub fn new() -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        PluginHost {
            next_task: Cell::new(1),
            done_tx,
            done_rx,
        }
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

/// A sync task, on a thread of its own. Everything it needs is the board's
/// folder on disk and the redeem token.
fn run_task(task: &str, _input: &[u8], token: Option<String>) -> Result<Vec<u8>, String> {
    let root = storage_dir().ok_or("the board has no folder")?;
    match task {
        TASK_SYNC => sicompass_sync::cloud::run_sync(&SERVICE, &root, token, &net_send),
        other => Err(format!("no task named `{other}`")),
    }
}

impl Host for PluginHost {
    fn now_millis(&self) -> u64 {
        host::now_millis()
    }

    fn standing(&self) -> Standing {
        let s = license::standing(SERVICE.tier);
        match s.status {
            TierStatus::Active => Standing::Active {
                renews_in_days: s.days,
            },
            TierStatus::Grace => Standing::Grace { days_left: s.days },
            TierStatus::Expired => Standing::Expired { days_ago: s.days },
            TierStatus::Missing => Standing::Missing,
        }
    }

    fn spawn(&self, task: &str, input: &[u8]) -> Result<u64, String> {
        let id = self.next_task.get();
        self.next_task.set(id + 1);
        let token = self.token();
        let (task, input, done) = (task.to_owned(), input.to_vec(), self.done_tx.clone());
        std::thread::Builder::new()
            .name(format!("projectmanagement-{task}"))
            .spawn(move || {
                let _ = done.send((id, run_task(&task, &input, token)));
            })
            .map_err(|e| e.to_string())?;
        Ok(id)
    }

    fn translate(&self, id: &str, args: &[(&str, String)]) -> String {
        translate(id, args)
    }
}

impl CloudHost for PluginHost {
    fn token(&self) -> Option<String> {
        license::token(SERVICE.tier)
    }

    fn finished(&self) -> Vec<TaskResult> {
        self.done_rx.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// A server for one request: it answers `status` with `body`, and hands
    /// back the request line, the headers and the body it was sent.
    fn one_shot_server(
        status: u16,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();
            let mut stream = reader.into_inner();
            write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            head + &String::from_utf8(sent).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn net_send_puts_the_body_with_the_headers() {
        let (url, server) = one_shot_server(200, r#"{"stored":true}"#);
        let resp = net_send(&Request {
            method: "PUT",
            url: format!("{url}/plugins/kanban"),
            headers: vec![("Authorization".to_owned(), "Bearer tok".to_owned())],
            body: Some(b"snapshot".to_vec()),
        })
        .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, br#"{"stored":true}"#);
        let seen = server.join().unwrap();
        assert!(seen.starts_with("PUT /plugins/kanban "), "{seen}");
        assert!(
            seen.to_ascii_lowercase()
                .contains("authorization: bearer tok"),
            "{seen}"
        );
        assert!(seen.ends_with("snapshot"), "{seen}");
    }

    /// The protocol reads the server's refusals itself, so an error status is
    /// an answer, not a failure: a 404 on a download means "nothing stored yet".
    #[test]
    fn net_send_hands_back_an_error_status_as_an_answer() {
        let (url, server) = one_shot_server(404, "");
        let resp = net_send(&Request {
            method: "GET",
            url: format!("{url}/plugins/kanban"),
            headers: Vec::new(),
            body: None,
        })
        .unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body.is_empty());
        assert!(server.join().unwrap().starts_with("GET /plugins/kanban "));
    }

    #[test]
    fn net_send_says_why_when_there_is_no_server() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = net_send(&Request {
            method: "GET",
            url: format!("http://127.0.0.1:{port}/plugins/kanban"),
            headers: Vec::new(),
            body: None,
        })
        .unwrap_err();
        assert!(err.contains("127.0.0.1"), "{err}");
    }

    /// A task runs on a thread, and its end arrives through `finished`, with
    /// the id `spawn` gave. Outside sicompass there is no board folder, so it
    /// fails, and says so.
    #[test]
    fn a_spawned_task_reports_back_through_finished() {
        let host = PluginHost::new();
        let first = host.spawn(TASK_SYNC, b"").unwrap();
        let second = host.spawn(TASK_SYNC, b"").unwrap();
        assert_ne!(first, second);

        let mut done = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while done.len() < 2 && std::time::Instant::now() < deadline {
            done.extend(host.finished());
            std::thread::sleep(Duration::from_millis(5));
        }
        done.sort_by_key(|(id, _)| *id);
        assert_eq!(done.len(), 2);
        assert_eq!(done[0].0, first);
        assert_eq!(done[1].0, second);
        for (_, result) in done {
            assert_eq!(result, Err("the board has no folder".to_owned()));
        }
        assert!(host.finished().is_empty());
    }
}
