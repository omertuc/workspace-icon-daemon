//! A minimal client for the i3/Sway IPC protocol.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;
const GET_VERSION: u32 = 7;
const GET_CONFIG: u32 = 9;
const EVENT_BIT: u32 = 1 << 31;
const EVENT_WORKSPACE: u32 = EVENT_BIT;
const EVENT_WINDOW: u32 = EVENT_BIT | 3;
const EVENT_BINDING: u32 = EVENT_BIT | 5;
const EVENT_SHUTDOWN: u32 = EVENT_BIT | 6;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WindowProperties {
    pub class: Option<String>,
}

/// A node of the layout tree, as returned by GET_TREE.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Node {
    pub id: i64,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub num: Option<i32>,
    pub layout: String,
    pub nodes: Vec<Node>,
    pub floating_nodes: Vec<Node>,
    pub focused: bool,
    pub rect: Rect,
    pub app_id: Option<String>,
    pub window_properties: Option<WindowProperties>,
    pub pid: Option<i64>,
}

impl Node {
    pub fn window_class(&self) -> Option<&str> {
        self.window_properties.as_ref()?.class.as_deref()
    }

    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("")
    }

    /// All nodes below this one, breadth first, each with its parent.
    fn descendants_with_parents(&self) -> Vec<(&Node, &Node)> {
        let mut result = Vec::new();
        let mut queue: VecDeque<(&Node, &Node)> = self
            .nodes
            .iter()
            .chain(&self.floating_nodes)
            .map(|child| (child, self))
            .collect();
        while let Some((node, parent)) = queue.pop_front() {
            result.push((node, parent));
            queue.extend(
                node.nodes
                    .iter()
                    .chain(&node.floating_nodes)
                    .map(|c| (c, node)),
            );
        }
        result
    }

    pub fn descendants(&self) -> Vec<&Node> {
        self.descendants_with_parents()
            .into_iter()
            .map(|(node, _)| node)
            .collect()
    }

    /// Workspaces below this node, leaving out internal ones like the scratchpad.
    pub fn workspaces(&self) -> Vec<&Node> {
        self.descendants()
            .into_iter()
            .filter(|node| node.kind == "workspace" && !node.name().starts_with("__"))
            .collect()
    }

    /// Windows below this node.
    pub fn leaves(&self) -> Vec<&Node> {
        self.descendants_with_parents()
            .into_iter()
            .filter(|(node, parent)| {
                node.nodes.is_empty() && node.kind == "con" && parent.kind != "dockarea"
            })
            .map(|(node, _)| node)
            .collect()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Version {
    pub human_readable: String,
    pub loaded_config_file_name: String,
}

#[derive(Debug)]
pub enum Event {
    Window {
        change: String,
        container: Box<Node>,
    },
    Workspace {
        change: String,
    },
    Binding,
    Shutdown,
}

pub struct Connection {
    stream: UnixStream,
}

/// The requests the daemon makes, so they can be faked in tests.
pub trait Ipc: Send {
    fn get_tree(&mut self) -> Result<Node>;
    fn command(&mut self, command: &str) -> Result<()>;
    fn get_config(&mut self) -> Result<String>;
}

impl Ipc for Connection {
    fn get_tree(&mut self) -> Result<Node> {
        Connection::get_tree(self)
    }

    fn command(&mut self, command: &str) -> Result<()> {
        Connection::command(self, command)
    }

    fn get_config(&mut self) -> Result<String> {
        Connection::get_config(self)
    }
}

fn socket_path() -> Result<String> {
    for variable in ["I3SOCK", "SWAYSOCK"] {
        if let Ok(path) = std::env::var(variable)
            && !path.is_empty()
        {
            return Ok(path);
        }
    }
    for program in ["i3", "sway"] {
        if let Ok(output) = Command::new(program).arg("--get-socketpath").output()
            && output.status.success()
        {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(path);
            }
        }
    }
    bail!("Could not find the i3/Sway IPC socket")
}

impl Connection {
    pub fn connect() -> Result<Self> {
        let path = socket_path()?;
        let stream = UnixStream::connect(&path).with_context(|| format!("Connecting to {path}"))?;
        Ok(Self { stream })
    }

    fn send(&mut self, kind: u32, payload: &[u8]) -> Result<()> {
        let mut message = Vec::with_capacity(14 + payload.len());
        message.extend_from_slice(MAGIC);
        message.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        message.extend_from_slice(&kind.to_ne_bytes());
        message.extend_from_slice(payload);
        self.stream.write_all(&message)?;
        Ok(())
    }

    /// The next message, or None once the compositor has closed the socket.
    fn receive(&mut self) -> Result<Option<(u32, Vec<u8>)>> {
        let mut header = [0u8; 14];
        match self.stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        if &header[..6] != MAGIC {
            bail!("Invalid IPC message");
        }
        let length = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
        let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
        let mut payload = vec![0u8; length];
        self.stream.read_exact(&mut payload)?;
        // Titles may hold invalid UTF-8; replace it rather than reject the
        // whole message.
        let payload = match String::from_utf8_lossy(&payload) {
            std::borrow::Cow::Borrowed(_) => payload,
            std::borrow::Cow::Owned(text) => text.into_bytes(),
        };
        Ok(Some((kind, payload)))
    }

    fn request(&mut self, kind: u32, payload: &[u8]) -> Result<Vec<u8>> {
        self.send(kind, payload)?;
        loop {
            match self.receive()? {
                Some((reply, data)) if reply == kind => return Ok(data),
                Some(_) => continue,
                None => bail!("IPC connection closed"),
            }
        }
    }

    pub fn command(&mut self, command: &str) -> Result<()> {
        let reply = self.request(RUN_COMMAND, command.as_bytes())?;
        let outcomes: Vec<serde_json::Value> = serde_json::from_slice(&reply)?;
        for outcome in outcomes {
            if outcome.get("success").and_then(|v| v.as_bool()) == Some(false) {
                log::debug!(
                    "Command failed: {command}: {}",
                    outcome.get("error").and_then(|v| v.as_str()).unwrap_or("")
                );
            }
        }
        Ok(())
    }

    pub fn get_tree(&mut self) -> Result<Node> {
        Ok(serde_json::from_slice(&self.request(GET_TREE, b"")?)?)
    }

    pub fn get_version(&mut self) -> Result<Version> {
        Ok(serde_json::from_slice(&self.request(GET_VERSION, b"")?)?)
    }

    pub fn get_config(&mut self) -> Result<String> {
        #[derive(Deserialize)]
        struct Config {
            config: String,
        }
        let config: Config = serde_json::from_slice(&self.request(GET_CONFIG, b"")?)?;
        Ok(config.config)
    }

    pub fn subscribe(mut self, events: &[&str]) -> Result<EventStream> {
        let reply = self.request(SUBSCRIBE, serde_json::to_string(events)?.as_bytes())?;
        let reply: serde_json::Value = serde_json::from_slice(&reply)?;
        if reply.get("success").and_then(|v| v.as_bool()) != Some(true) {
            bail!("Could not subscribe to IPC events");
        }
        Ok(EventStream { connection: self })
    }
}

pub struct EventStream {
    connection: Connection,
}

impl EventStream {
    /// The next event, or None when the compositor goes away.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        #[derive(Deserialize)]
        struct WindowEvent {
            change: String,
            container: Box<Node>,
        }
        #[derive(Deserialize)]
        struct WorkspaceEvent {
            change: String,
        }
        loop {
            let Some((kind, payload)) = self.connection.receive()? else {
                return Ok(None);
            };
            let event = match kind {
                EVENT_WINDOW => {
                    serde_json::from_slice::<WindowEvent>(&payload).map(|event| Event::Window {
                        change: event.change,
                        container: event.container,
                    })
                }
                EVENT_WORKSPACE => {
                    serde_json::from_slice::<WorkspaceEvent>(&payload).map(|event| {
                        Event::Workspace {
                            change: event.change,
                        }
                    })
                }
                EVENT_BINDING => Ok(Event::Binding),
                EVENT_SHUTDOWN => Ok(Event::Shutdown),
                _ => continue,
            };
            match event {
                Ok(event) => return Ok(Some(event)),
                // One unreadable event must not end the daemon.
                Err(error) => log::warn!("Skipping unreadable IPC event: {error}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_in_payloads_is_replaced() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut connection = Connection { stream: a };
        let payload = b"{\"name\":\"bad \xff title\"}";
        let mut message = MAGIC.to_vec();
        message.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        message.extend_from_slice(&GET_TREE.to_ne_bytes());
        message.extend_from_slice(payload);
        b.write_all(&message).unwrap();
        let (_, data) = connection.receive().unwrap().unwrap();
        let node: Node = serde_json::from_slice(&data).unwrap();
        assert_eq!(node.name(), "bad \u{FFFD} title");
    }

    #[test]
    fn leaves_and_workspaces_follow_i3ipc_semantics() {
        let tree: Node = serde_json::from_str(
            r#"{"id":1,"type":"root","nodes":[{"id":2,"type":"output","nodes":[
                {"id":3,"type":"workspace","name":"__i3_scratch","nodes":[]},
                {"id":4,"type":"workspace","name":"1","num":1,"nodes":[
                    {"id":5,"type":"con","layout":"splitv","nodes":[
                        {"id":6,"type":"con","app_id":"foot","nodes":[]},
                        {"id":7,"type":"con","window_properties":{"class":"Firefox"},"nodes":[]}]},
                    {"id":8,"type":"con","nodes":[]}],
                 "floating_nodes":[{"id":9,"type":"floating_con","nodes":[]}]}]}]}"#,
        )
        .unwrap();
        let workspaces = tree.workspaces();
        assert_eq!(workspaces.iter().map(|w| w.id).collect::<Vec<_>>(), [4]);
        let leaves: Vec<i64> = workspaces[0].leaves().iter().map(|n| n.id).collect();
        assert_eq!(leaves, [8, 6, 7]);
        assert_eq!(workspaces[0].leaves()[2].window_class(), Some("Firefox"));
    }
}
