//! Schemas shared by everything that speaks for the ilex installer: the host
//! tooling that writes the composition table, the root helper
//! (`ilex-installerd`) that probes, plans and installs, and the interfaces that
//! drive it. No platform code lives here.

pub mod answers;
pub mod catalogue;
pub mod detect;
pub mod edition;
pub mod finish;
pub mod plan;
pub mod probe;
pub mod recovery;
pub mod selfcheck;
pub mod state;

pub use catalogue::{
    Composition, DMI_FIELDS, DmiMatch, Machine, Profile, REPOSITORY, Shell, Table, validate_tag,
};
pub use detect::{Detection, Identity, ProfileChoice, Selected, select};
pub use edition::Edition;
