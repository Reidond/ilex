//! The network page: shown after the welcome page when the live system cannot
//! reach the image registry, and left out of the walk when it can (the wired
//! DHCP case).
//!
//! ```text
//! welcome   advance() ------------> Step::RunNetwork { Look }       (Continue says Checking…)
//!           network_finished(..) -> the machine page when the registry answers,
//!                                   otherwise the network page and Step::RunNetwork (a poll)
//! network   network_finished(..) -> a poll again every few seconds while the page shows,
//!                                   so a cable plugged in or a joined network shows at once
//!           connect_network() ----> Step::RunNetwork { Join }       (the password goes with it)
//!           join_finished(..) ----> the flow wipes the password, then looks again
//! ```
//!
//! The person goes on with Continue once the page says the registry can be
//! reached; nothing moves the flow by itself. The Wi-Fi password is a
//! [`Secret`]: it is wiped when the attempt ends, whichever way, when another
//! network is chosen and with the other passwords.

use super::{Flow, PageId, Step};
use crate::markers;
use crate::messages::{self, ClientError, Message};
use crate::network::{self, NetworkState, Security};
use crate::secret::Secret;

/// How often the network is looked at while its page shows.
const POLL_MS: u64 = 3000;

/// What a worker asks NetworkManager.
#[derive(Clone, Debug, PartialEq)]
pub enum NetworkAction {
    /// Look at the network (`rescan`: scan for Wi-Fi networks again now).
    Look { rescan: bool },
    /// Join a Wi-Fi network.
    Join {
        ssid: String,
        security: Security,
        password: Option<Secret>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The welcome page's Continue.
    Advance,
    Poll,
    Refresh,
    Join,
}

#[derive(Default)]
enum Join {
    #[default]
    Idle,
    Joining(String),
    Failed(Message),
}

#[derive(Default)]
pub(super) struct Net {
    /// The welcome page's Continue looks at the network first (an interface
    /// turns this on; the flow's own checks mostly leave it off).
    pub(super) enabled: bool,
    generation: u64,
    in_flight: Option<(u64, Kind)>,
    state: Option<NetworkState>,
    /// The network page is part of this walk (the welcome page's look found
    /// the registry out of reach).
    walk: bool,
    selected: Option<String>,
    password: Secret,
    join: Join,
}

impl Net {
    pub(super) fn clear_password(&mut self) {
        self.password = Secret::default();
    }
}

impl Flow {
    /// Have the welcome page's Continue look at the network first, and show the
    /// network page when the registry cannot be reached.
    pub fn with_network_check(mut self) -> Self {
        self.net.enabled = true;
        self
    }

    fn begin_network(&mut self, kind: Kind, action: NetworkAction, delay_ms: u64) -> Step {
        self.net.generation += 1;
        self.net.in_flight = Some((self.net.generation, kind));
        Step::RunNetwork {
            generation: self.net.generation,
            delay_ms,
            action,
        }
    }

    /// The welcome page's Continue: look at the network first.
    pub(super) fn begin_network_check(&mut self) -> Step {
        self.begin_network(Kind::Advance, NetworkAction::Look { rescan: false }, 0)
    }

    fn poll_network(&mut self, delay_ms: u64) -> Step {
        self.begin_network(Kind::Poll, NetworkAction::Look { rescan: false }, delay_ms)
    }

    /// The network page is showing again (Back from the next page): look at
    /// the network every few seconds again.
    pub(crate) fn resume_network(&mut self) -> Step {
        if self.page == PageId::Network && self.net.in_flight.is_none() {
            self.poll_network(POLL_MS)
        } else {
            Step::Stay
        }
    }

    /// The welcome page's Continue is waiting for the look, or Refresh is.
    pub fn network_pending(&self) -> bool {
        matches!(
            self.net.in_flight,
            Some((_, Kind::Advance | Kind::Refresh))
        )
    }

    /// Continue on the welcome page is waiting for the look at the network.
    pub(super) fn network_advancing(&self) -> bool {
        matches!(self.net.in_flight, Some((_, Kind::Advance)))
    }

    fn joining(&self) -> Option<&str> {
        match &self.net.join {
            Join::Joining(ssid) => Some(ssid),
            _ => None,
        }
    }

    /// May Continue leave the network page: the registry can be reached.
    pub(super) fn network_online(&self) -> bool {
        self.joining().is_none() && self.net.state.as_ref().is_some_and(|s| s.online)
    }

    /// The pages the navigation holds for the page showing: the network page
    /// only when this walk went through it.
    pub fn stack(&self) -> Vec<PageId> {
        let mut stack = self.page.stack();
        if !self.net.walk {
            stack.retain(|p| *p != PageId::Network);
        }
        stack
    }

    /// The page before this one in this walk.
    pub(super) fn previous_page(&self) -> Option<PageId> {
        match self.page.previous()? {
            PageId::Network if !self.net.walk => Some(PageId::Welcome),
            page => Some(page),
        }
    }

    /// The answer to a [`Step::RunNetwork`] that looked. One for any other
    /// request changes nothing.
    pub fn network_finished(
        &mut self,
        generation: u64,
        result: Result<NetworkState, ClientError>,
    ) -> Step {
        let Some((current, kind)) = self.net.in_flight else {
            self.say(markers::stale("network"));
            return Step::Stay;
        };
        if current != generation || kind == Kind::Join {
            self.say(markers::stale("network"));
            return Step::Stay;
        }
        self.net.in_flight = None;
        // A poll says so only when something changed (every three seconds
        // would fill the console).
        if kind != Kind::Poll || result.as_ref().map_or(true, |s| self.net.state.as_ref() != Some(s)) {
            self.say(markers::network(&result));
        }
        match (kind, result) {
            (Kind::Advance, Err(_)) => {
                // The look failed, not the network: do not hold the person
                // back; a registry out of reach is said where it matters.
                self.net.walk = false;
                self.go(PageId::Machine)
            }
            (Kind::Advance, Ok(state)) => {
                let online = state.online;
                self.net.state = Some(state);
                self.net.walk = !online;
                if online {
                    return self.go(PageId::Machine);
                }
                self.go(PageId::Network);
                self.poll_network(POLL_MS)
            }
            (_, Ok(state)) => {
                self.net.state = Some(state);
                // A selection that is gone from the list is dropped.
                if let Some(ssid) = &self.net.selected
                    && !self.net_has(ssid)
                {
                    self.net.selected = None;
                }
                if self.page == PageId::Network {
                    self.poll_network(POLL_MS)
                } else {
                    Step::Stay
                }
            }
            (_, Err(_)) => {
                if self.page == PageId::Network {
                    self.poll_network(POLL_MS)
                } else {
                    Step::Stay
                }
            }
        }
    }

    /// The answer to a [`Step::RunNetwork`] that joined a Wi-Fi network.
    pub fn join_finished(&mut self, generation: u64, result: Result<(), ClientError>) -> Step {
        if self.net.in_flight != Some((generation, Kind::Join)) {
            self.say(markers::stale("wifi"));
            return Step::Stay;
        }
        self.net.in_flight = None;
        self.say(markers::wifi(result.is_ok()));
        let ssid = self.joining().unwrap_or_default().to_owned();
        // Whatever happened, the password is not kept.
        self.net.password = Secret::default();
        self.secrets_wiped += 1;
        self.net.join = match result {
            Ok(()) => Join::Idle,
            Err(_) => Join::Failed(messages::wifi_failed(&ssid)),
        };
        self.poll_network(0)
    }

    fn net_has(&self, ssid: &str) -> bool {
        self.net
            .state
            .as_ref()
            .is_some_and(|s| s.networks.iter().any(|n| n.ssid == ssid))
    }

    fn selected_network(&self) -> Option<&network::Wifi> {
        let ssid = self.net.selected.as_deref()?;
        self.net
            .state
            .as_ref()?
            .networks
            .iter()
            .find(|n| n.ssid == ssid)
    }

    /// A Wi-Fi network was chosen from the list.
    pub fn select_network(&mut self, ssid: &str) {
        if self.page != PageId::Network || self.joining().is_some() {
            return;
        }
        let offered = self
            .net
            .state
            .as_ref()
            .and_then(|s| s.networks.iter().find(|n| n.ssid == ssid))
            .is_some_and(|n| n.security != Security::Unsupported);
        if !offered || self.net.selected.as_deref() == Some(ssid) {
            return;
        }
        self.net.selected = Some(ssid.to_owned());
        self.net.join = Join::Idle;
        if !self.net.password.is_empty() {
            self.net.password = Secret::default();
            self.secrets_wiped += 1;
        }
    }

    /// The Wi-Fi password field changed.
    pub fn set_network_password(&mut self, password: Secret) {
        // (A field the interface cleared after a wipe says the same again.)
        if self.page != PageId::Network
            || self.joining().is_some()
            || password == self.net.password
        {
            return;
        }
        self.net.password = password;
        if matches!(self.net.join, Join::Failed(_)) {
            self.net.join = Join::Idle;
        }
    }

    /// What is typed in the Wi-Fi password field, as the flow holds it (a page
    /// that comes back puts it in its field again).
    pub fn network_password(&self) -> &Secret {
        &self.net.password
    }

    /// May Connect be pressed: a network is chosen and, when it has one, its
    /// password fits.
    pub fn can_connect(&self) -> bool {
        self.page == PageId::Network
            && self.joining().is_none()
            && self.selected_network().is_some_and(|n| {
                !n.security.needs_password() || network::passphrase_fits(&self.net.password)
            })
    }

    /// Connect was pressed.
    pub fn connect_network(&mut self) -> Step {
        if !self.can_connect() {
            return Step::Stay;
        }
        let Some(network) = self.selected_network().cloned() else {
            return Step::Stay;
        };
        let password = network
            .security
            .needs_password()
            .then(|| self.net.password.clone());
        self.net.join = Join::Joining(network.ssid.clone());
        self.begin_network(
            Kind::Join,
            NetworkAction::Join {
                ssid: network.ssid,
                security: network.security,
                password,
            },
            0,
        )
    }

    /// Refresh was pressed: scan for Wi-Fi networks again.
    pub fn refresh_network(&mut self) -> Step {
        if self.page != PageId::Network || self.joining().is_some() {
            return Step::Stay;
        }
        self.begin_network(Kind::Refresh, NetworkAction::Look { rescan: true }, 0)
    }

    /// The word the network page's serial marker and the renderer carry.
    pub fn network_word(&self) -> &'static str {
        match &self.net.join {
            Join::Joining(_) => "joining",
            Join::Failed(_) => "failed",
            Join::Idle => {
                if self.net.state.as_ref().is_some_and(|s| s.online) {
                    "online"
                } else {
                    "offline"
                }
            }
        }
    }

    pub(super) fn network_marker(&self) -> String {
        match &self.net.state {
            Some(state) => format!(
                "check={} wired={} wifi={} networks={}",
                self.network_word(),
                markers::wired_word(state.wired),
                state.wifi,
                state.networks.len()
            ),
            None => "check=pending".to_owned(),
        }
    }

    /// What the network page is made of, for [`Flow::view`].
    pub(crate) fn network_data(&self) -> NetData<'_> {
        NetData {
            state: self.net.state.as_ref(),
            selected: self.selected_network(),
            joining: self.joining(),
            failed: match &self.net.join {
                Join::Failed(message) => Some(message),
                _ => None,
            },
            pending: self.network_pending(),
        }
    }
}

/// See [`Flow::network_data`].
pub(crate) struct NetData<'a> {
    pub state: Option<&'a NetworkState>,
    pub selected: Option<&'a network::Wifi>,
    pub joining: Option<&'a str>,
    pub failed: Option<&'a Message>,
    pub pending: bool,
}
