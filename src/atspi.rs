//! Which sites browser windows show, read from their address bars over
//! AT-SPI (the accessibility bus).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use zbus::blocking::Connection;
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Type};

use crate::favicons::{browser_family, page_title};

const ROLE_COMBO_BOX: u32 = 11;
const ROLE_DOCUMENT_WEB: u32 = 95;
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const TEXT: &str = "org.a11y.atspi.Text";

/// An accessible object: its bus name and object path.
type Accessible = (String, OwnedObjectPath);

fn connect() -> Result<Connection> {
    let session = Connection::session().context("connecting to the session bus")?;
    let reply = session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )
        .context("asking for the accessibility bus address")?;
    let address: String = reply
        .body()
        .deserialize()
        .context("reading the accessibility bus address")?;
    zbus::blocking::connection::Builder::address(address.as_str())
        .with_context(|| format!("parsing accessibility bus address {address:?}"))?
        .method_timeout(Duration::from_secs(2))
        .build()
        .with_context(|| format!("connecting to the accessibility bus at {address}"))
}

fn call<B, R>(bus: &Connection, node: &Accessible, iface: &str, method: &str, body: &B) -> Option<R>
where
    B: serde::Serialize + DynamicType,
    R: DeserializeOwned + Type,
{
    let reply = bus
        .call_method(
            Some(node.0.as_str()),
            node.1.as_str(),
            Some(iface),
            method,
            body,
        )
        .ok()?;
    reply.body().deserialize().ok()
}

fn name(bus: &Connection, node: &Accessible) -> Option<String> {
    let value: OwnedValue = call(
        bus,
        node,
        "org.freedesktop.DBus.Properties",
        "Get",
        &(ACCESSIBLE, "Name"),
    )?;
    String::try_from(value).ok()
}

fn children(bus: &Connection, node: &Accessible) -> Vec<Accessible> {
    call(bus, node, ACCESSIBLE, "GetChildren", &()).unwrap_or_default()
}

/// Depth-first search for the address bar, skipping page content.
fn address_bar_text(bus: &Connection, window: Accessible) -> Option<String> {
    let mut stack = vec![window];
    while let Some(node) = stack.pop() {
        // Nodes vanish while being walked; those are skipped.
        let Some(role) = call::<_, u32>(bus, &node, ACCESSIBLE, "GetRole", &()) else {
            continue;
        };
        if role == ROLE_COMBO_BOX {
            return call(bus, &node, TEXT, "GetText", &(0i32, -1i32));
        }
        if role != ROLE_DOCUMENT_WEB {
            stack.extend(children(bus, &node));
        }
    }
    None
}

/// The host of an address bar value, which Firefox shows without a scheme.
pub fn host(address: Option<&str>) -> Option<String> {
    let address = address?.trim();
    if address.is_empty() || address.contains(' ') {
        return None;
    }
    let rest = match address.split_once("://") {
        Some((_, rest)) => rest,
        None => address,
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);
    (!host.is_empty() && (host.contains('.') || host.contains(':'))).then(|| host.to_string())
}

/// Map each browser window's (browser family, page title) to the host in
/// its address bar.
pub fn address_bar_hosts() -> HashMap<(String, String), Option<String>> {
    match read_address_bar_hosts() {
        Ok(hosts) => hosts,
        Err(error) => {
            log::debug!("AT-SPI unavailable: {error:#}");
            HashMap::new()
        }
    }
}

fn is_browser_app(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("firefox") || name.contains("chrom")
}

fn read_address_bar_hosts() -> Result<HashMap<(String, String), Option<String>>> {
    let mut hosts: HashMap<(String, String), Option<String>> = HashMap::new();
    let bus = connect().context("connecting to AT-SPI")?;
    let root: Accessible = (
        "org.a11y.atspi.Registry".to_string(),
        OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root")
            .context("parsing the registry root path")?,
    );
    let mut conflicting: HashSet<(String, String)> = HashSet::new();
    for app in children(&bus, &root) {
        let Some(app_name) = name(&bus, &app).filter(|n| is_browser_app(n)) else {
            continue;
        };
        for window in children(&bus, &app) {
            let Some(window_name) = name(&bus, &window).filter(|n| !n.is_empty()) else {
                continue;
            };
            let title = page_title(&window_name).context("reading the page title")?;
            if title == window_name {
                continue; // No page title yet, e.g. a new or loading window.
            }
            let key = (browser_family(&app_name).to_string(), title);
            let mut site = host(address_bar_text(&bus, window).as_deref());
            // Windows sharing a title can't be told apart, so if they show
            // different sites none of them gets a favicon.
            if conflicting.contains(&key) || hosts.get(&key).is_some_and(|h| *h != site) {
                conflicting.insert(key.clone());
                site = None;
            }
            hosts.insert(key, site);
        }
    }
    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_from_address_bars() {
        assert_eq!(
            host(Some("github.com/foo/bar")).as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host(Some("https://user@example.org:8080/x?y")).as_deref(),
            Some("example.org:8080")
        );
        assert_eq!(host(Some("Search with Google or enter address")), None);
        assert_eq!(host(Some("localhost")), None);
        assert_eq!(host(Some("")), None);
        assert_eq!(host(None), None);
    }
}
