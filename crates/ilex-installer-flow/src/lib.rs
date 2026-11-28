//! The ilex installer's flow, shared by every interface. The GNOME edition's
//! GTK 4 + libadwaita application (`ilex-installer`) renders it today and the
//! KDE edition's Kirigami interface (through cxx-qt) will drive the same
//! crate: nothing here knows a toolkit, a compositor or a thread.
//!
//! What lives here:
//!
//! * [`flow`]: the pages in order, what each needs before Continue works, what
//!   Continue does ([`flow::Flow`], a state machine with no I/O). Every rule an
//!   interface follows is here: answers to the helper and the compositor are
//!   numbered, stale ones are ignored, the layout switch is serialised.
//! * [`backend`]: the root helper as a trait ([`backend::Backend`]), with the
//!   real implementation over D-Bus (`org.ilex.Installer1`, Linux) and
//!   [`simulate::Simulated`], which answers from JSON fixtures.
//! * [`recovery`]: the recovery key page and the security chip's row: wording and rules.
//! * [`machine`], [`disks`], [`encryption`], [`account`], [`daily`], [`review`], [`keyboard`], [`language`]:
//!   the content and rules of the pages made of what the helper reports and what
//!   the live image ships.
//! * [`network`]: the live system's network (NetworkManager through `nmcli`) and the
//!   network page's data.
//! * [`messages`]: every sentence a refusal or a failure becomes, in one place.
//! * [`markers`]: the serial-console lines the trials assert on, in one place.
//! * [`progress`]: the installation's stages as rows, from the helper's state.
//! * [`selfcheck`]: the rules exercised on the fixtures, for `--self-check`.
//!
//! `ilex-installer-model` stays the one definition of the schemas and of every
//! rule the helper also applies (names, tags, detection, the request checks);
//! this crate adds only what an interface needs on top.

pub mod account;
pub mod backend;
pub mod daily;
pub mod disks;
pub mod encryption;
pub mod events;
pub mod flow;
pub mod keyboard;
pub mod language;
pub mod machine;
pub mod markers;
pub mod messages;
pub mod network;
pub mod progress;
pub mod recovery;
pub mod review;
pub mod runner;
pub mod secret;
pub mod selfcheck;
pub mod shots;
pub mod simulate;
pub mod view;

pub use backend::Backend;
pub use flow::{Flow, PageId, Step};
pub use messages::{ClientError, Message};
pub use secret::Secret;
