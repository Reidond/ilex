//! The live system's network, as the network page needs it: whether the image
//! registry can be reached, the wired adapter's state and the Wi-Fi networks in
//! range, read from NetworkManager through `nmcli` (which the live image has,
//! with its Wi-Fi support), and joining a Wi-Fi network.
//!
//! The installer user runs `nmcli` itself: the polkit rule of the live image
//! (`00-ilex-installer.rules`) names the few NetworkManager actions it needs.
//! A Wi-Fi password reaches `nmcli` on its standard input (`passwd-file
//! /dev/stdin`), never on a command line, in the environment or in a file, and
//! nothing here prints it.

use serde::Serialize;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::Duration;
use zeroize::Zeroizing;

use crate::secret::Secret;

/// What the network is checked against: the registry the image is pulled from.
pub const REGISTRY_HOST: &str = "ghcr.io";

/// The profile NetworkManager keeps for the one Wi-Fi network joined here.
const PROFILE: &str = "ilex-wifi";

/// A WPA passphrase is 8 to 63 characters (or 64 hexadecimal digits for the key
/// itself).
pub const PASSPHRASE_MIN: usize = 8;
const PASSPHRASE_MAX: usize = 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Wired {
    /// This computer has no wired adapter.
    None,
    /// An adapter without a cable (no carrier).
    Unplugged,
    /// A cable, and an address is being asked for (or the connection is not
    /// up yet).
    Starting,
    /// The connection is up.
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    Open,
    /// WPA or WPA2 with a passphrase (and WPA2/WPA3 mixed networks).
    Wpa,
    /// WPA3 with a passphrase only.
    Wpa3,
    /// WEP, enterprise (802.1X) and the rest: not offered.
    Unsupported,
}

impl Security {
    fn of(nmcli: &str) -> Self {
        let words: Vec<&str> = nmcli.split_whitespace().collect();
        if words.is_empty() || nmcli == "--" {
            Self::Open
        } else if words
            .iter()
            .any(|w| w.contains("802.1X") || *w == "WEP" || w.starts_with("OWE"))
        {
            Self::Unsupported
        } else if words.iter().any(|w| matches!(*w, "WPA" | "WPA1" | "WPA2")) {
            Self::Wpa
        } else if words.contains(&"WPA3") {
            Self::Wpa3
        } else {
            Self::Unsupported
        }
    }

    /// Does joining it need a password?
    pub fn needs_password(self) -> bool {
        matches!(self, Self::Wpa | Self::Wpa3)
    }
}

/// Is `password` one a WPA network can have?
pub fn passphrase_fits(password: &Secret) -> bool {
    let text = password.expose();
    let chars = text.chars().count();
    (PASSPHRASE_MIN..=PASSPHRASE_MAX).contains(&chars)
        || (chars == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Wifi {
    pub ssid: String,
    /// 0 to 100.
    pub signal: u8,
    pub security: Security,
    /// This computer is joined to it.
    pub in_use: bool,
}

/// What the network page knows.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkState {
    pub wired: Wired,
    /// This computer has a Wi-Fi adapter.
    pub wifi: bool,
    /// The registry can be reached.
    pub online: bool,
    /// The networks in range, the strongest first, one per name.
    pub networks: Vec<Wifi>,
}

impl NetworkState {
    /// The network joined, if any.
    pub fn joined(&self) -> Option<&Wifi> {
        self.networks.iter().find(|n| n.in_use)
    }
}

// --- nmcli -------------------------------------------------------------------

/// Run `nmcli -t` with `args` (and `stdin`, when given): its standard output.
/// `Err` carries only the exit status, never what nmcli printed.
fn nmcli(args: &[&str], stdin: Option<&[u8]>) -> Result<String, String> {
    let mut command = Command::new("nmcli");
    command
        .args(["-c", "no", "-t"])
        .args(args)
        .env("LC_ALL", "C")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|e| format!("nmcli: {e}"))?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // Small, so it fits the pipe: nmcli reads it when it needs it.
        let _ = pipe.write_all(bytes);
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("nmcli: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!("nmcli exited with {}", output.status))
    }
}

/// A `-t` line's fields: `:` separates them, `\:` and `\\` are the characters.
fn fields(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let (Some(next), Some(field)) = (chars.next(), out.last_mut()) {
                    field.push(next);
                }
            }
            ':' => out.push(String::new()),
            c => {
                if let Some(field) = out.last_mut() {
                    field.push(c);
                }
            }
        }
    }
    out
}

/// Can the registry be reached? The name resolves and the port answers, within
/// a few seconds (a resolver whose servers do not answer takes longer than
/// anyone waits; its thread is left to give up by itself).
fn registry_answers() -> bool {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let answers = format!("{REGISTRY_HOST}:443")
            .to_socket_addrs()
            .is_ok_and(|mut addresses| {
                addresses.any(|a| TcpStream::connect_timeout(&a, Duration::from_secs(4)).is_ok())
            });
        let _ = sender.send(answers);
    });
    receiver
        .recv_timeout(Duration::from_secs(8))
        .unwrap_or(false)
}

/// Look at the network. `rescan` asks the Wi-Fi adapter to scan again now
/// (Refresh) instead of when its last scan is old.
pub fn look(rescan: bool) -> NetworkState {
    let mut wired = Wired::None;
    let mut wifi = false;
    if let Ok(text) = nmcli(&["-f", "TYPE,STATE", "device", "status"], None) {
        for line in text.lines() {
            let f = fields(line);
            let (Some(kind), Some(state)) = (f.first(), f.get(1)) else {
                continue;
            };
            match kind.as_str() {
                "ethernet" if state == "unmanaged" => {}
                "ethernet" => {
                    let now = match state.as_str() {
                        "connected" => Wired::Up,
                        "unavailable" => Wired::Unplugged,
                        _ => Wired::Starting,
                    };
                    // Several adapters: the best state.
                    wired = match (wired, now) {
                        (Wired::Up, _) | (_, Wired::Up) => Wired::Up,
                        (Wired::Starting, _) | (_, Wired::Starting) => Wired::Starting,
                        _ => Wired::Unplugged,
                    };
                }
                "wifi" => wifi = true,
                _ => {}
            }
        }
    }
    let online = registry_answers();
    let mut networks: Vec<Wifi> = Vec::new();
    if wifi {
        let rescan = if rescan { "yes" } else { "auto" };
        let list = nmcli(
            &[
                "-f",
                "IN-USE,SSID,SIGNAL,SECURITY",
                "device",
                "wifi",
                "list",
                "--rescan",
                rescan,
            ],
            None,
        );
        for line in list.unwrap_or_default().lines() {
            let f = fields(line);
            if f.len() < 4 || f[1].is_empty() {
                continue;
            }
            let network = Wifi {
                ssid: f[1].clone(),
                signal: f[2].parse::<u8>().unwrap_or(0).min(100),
                security: Security::of(&f[3]),
                in_use: f[0] == "*",
            };
            match networks.iter_mut().find(|n| n.ssid == network.ssid) {
                Some(have) if have.in_use || (!network.in_use && have.signal >= network.signal) => {
                }
                Some(have) => *have = network,
                None => networks.push(network),
            }
        }
        networks.sort_by(|a, b| b.in_use.cmp(&a.in_use).then(b.signal.cmp(&a.signal)));
    }
    NetworkState {
        wired,
        wifi,
        online,
        networks,
    }
}

/// Join a Wi-Fi network: a profile for it, then the activation with the
/// password on `nmcli`'s standard input. A failed attempt leaves no profile
/// (NetworkManager would keep retrying a wrong password).
pub fn join(ssid: &str, security: Security, password: Option<&Secret>) -> Result<(), String> {
    let key_management = match security {
        Security::Open => None,
        Security::Wpa => Some("wpa-psk"),
        Security::Wpa3 => Some("sae"),
        Security::Unsupported => return Err("this network cannot be joined".to_owned()),
    };
    let _ = nmcli(&["connection", "delete", "id", PROFILE], None);
    let mut add = vec![
        "connection",
        "add",
        "type",
        "wifi",
        "ifname",
        "*",
        "con-name",
        PROFILE,
        "ssid",
        ssid,
    ];
    if let Some(key_management) = key_management {
        add.extend(["wifi-sec.key-mgmt", key_management]);
    }
    nmcli(&add, None)?;
    let result = match (key_management, password) {
        (Some(_), Some(password)) => {
            let mut line = Zeroizing::new(Vec::new());
            line.extend_from_slice(b"802-11-wireless-security.psk:");
            line.extend_from_slice(password.expose().as_bytes());
            line.push(b'\n');
            nmcli(
                &[
                    "-w",
                    "45",
                    "connection",
                    "up",
                    "id",
                    PROFILE,
                    "passwd-file",
                    "/dev/stdin",
                ],
                Some(line.as_slice()),
            )
        }
        (Some(_), None) => Err("the network needs a password".to_owned()),
        (None, _) => nmcli(&["-w", "45", "connection", "up", "id", PROFILE], None),
    };
    if result.is_err() {
        let _ = nmcli(&["connection", "delete", "id", PROFILE], None);
    }
    result.map(|_| ())
}
