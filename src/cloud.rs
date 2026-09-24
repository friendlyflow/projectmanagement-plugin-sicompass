//! Cloud backup of the board, to Sicompass Cloud.
//!
//! Off until the user ticks "enable cloud backup" in the board's settings. The
//! service itself (the switch, uploads a while after the last change and
//! restores, both as background tasks) is `sicompass_payments::cloud`, the
//! same one a third party's plugin would use. What is here is what only the
//! board knows: which service, the row above the columns, and the host inside
//! the sandbox.
//!
//! - **Where the user stands** comes from the host (`license.standing` for
//!   Sicompass Cloud), which verified the certificate. The row says it, in the
//!   user's language, and never links anywhere: buying and redeeming are in
//!   store, tiers.
//! - **An upload** is a task: a fresh instance of this plugin reads `/storage`,
//!   gets the redeem token from `license.token` (which the host gives only for
//!   this plugin's own service) and uploads through `net`.
//!
//! The paywall is on the service, never on the data: whatever the standing,
//! the board is shown and saved to disk. Only the copy on the server is paid.

use sicompass_payments::cloud::Service;
pub use sicompass_payments::cloud::{Cloud, Finished, Host, TASK_BACKUP, TASK_RESTORE};
use sicompass_payments::protocol::{Request, Response};
use sicompass_payments::row::Standing;
use sicompass_sdk::ffon::FfonElement;
use sicompass_sdk::tags;

use crate::localize;

/// Sicompass Cloud, for the board. `plugin.json` names the same tier as its
/// `service` and allows only this server. The store is called "kanban" on the
/// server because that is what it holds, and existing backups are under it.
pub const SERVICE: Service = Service {
    tier: "friendlyflow/cloud",
    server: "https://store.sicompass.org",
    store: "kanban",
    enable_key: ENABLE_KEY,
    prefix: "projectmanagement",
};

/// The settings key of the "enable cloud backup" switch.
pub const ENABLE_KEY: &str = "kanbanCloudBackup";

/// The `<id>` the backup row carries, so it is never taken for a column
/// (column and card ids are numbers).
pub const ROW_ID: &str = "cloud";

/// The host, plus the one thing only a task needs: the redeem token.
pub trait CloudHost: Host {
    /// The redeem token for [`SERVICE`]'s tier, which the host gives only
    /// because `plugin.json` names that tier as this plugin's service.
    fn token(&self) -> Option<String>;
}

/// The row above the columns, while the switch is on.
pub fn row(cloud: &Cloud, host: &dyn Host) -> Option<FfonElement> {
    let text = cloud.row_text(host)?;
    Some(FfonElement::new_str(format!(
        "{}{text}",
        tags::format_id(ROW_ID)
    )))
}

/// Whether a row is the backup row (rendered, never stored).
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

/// `send` over the plugin's `net` interface.
#[cfg(target_arch = "wasm32")]
pub fn net_send(req: &Request) -> Result<Response, String> {
    use sicompass_pdk::net;
    let resp = net::fetch(&net::HttpRequest {
        method: req.method.to_owned(),
        url: req.url.clone(),
        headers: req.headers.clone(),
        body: req.body.clone(),
    })?;
    Ok(Response {
        status: resp.status,
        body: resp.body,
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn net_send(_req: &Request) -> Result<Response, String> {
    Err("no network outside the sandbox".to_owned())
}

/// The host, inside the sandbox.
pub struct PluginHost;

#[cfg(target_arch = "wasm32")]
impl Host for PluginHost {
    fn now_millis(&self) -> u64 {
        sicompass_pdk::host::now_millis()
    }

    fn standing(&self) -> Standing {
        use sicompass_pdk::license::{self, TierStatus};
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
        sicompass_pdk::tasks::spawn(task, input)
    }

    fn translate(&self, id: &str, args: &[(&str, String)]) -> String {
        translate(id, args)
    }
}

#[cfg(target_arch = "wasm32")]
impl CloudHost for PluginHost {
    fn token(&self) -> Option<String> {
        sicompass_pdk::license::token(SERVICE.tier)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Host for PluginHost {
    fn now_millis(&self) -> u64 {
        0
    }

    fn standing(&self) -> Standing {
        Standing::Missing
    }

    fn spawn(&self, _task: &str, _input: &[u8]) -> Result<u64, String> {
        Err("no tasks outside the sandbox".to_owned())
    }

    fn translate(&self, id: &str, args: &[(&str, String)]) -> String {
        translate(id, args)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl CloudHost for PluginHost {
    fn token(&self) -> Option<String> {
        None
    }
}
