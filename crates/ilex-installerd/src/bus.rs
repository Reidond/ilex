//! `org.ilex.Installer1` on the system bus. The bus policy decides who may call
//! it (`ilex-installer` and root); every method takes JSON text and returns
//! JSON text, and none takes a path, an image reference, a kernel argument or a
//! command. A refusal's error message is the `PlanError` document.

use crate::helper::Helper;
use anyhow::Result;
use ilex_installer_model::plan::{ErrorCode, PlanError};
use std::sync::Arc;
use zbus::{connection, interface};

pub const NAME: &str = "org.ilex.Installer1";
pub const PATH: &str = "/org/ilex/Installer1";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.ilex.Installer1.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    /// The request is not valid JSON of this schema, or a field is malformed.
    InvalidRequest(String),
    /// A valid request the helper will not carry out.
    Refused(String),
    /// A service the plan needs (the registry) did not answer.
    Unavailable(String),
    Failed(String),
}

impl From<PlanError> for Error {
    fn from(error: PlanError) -> Self {
        let document = serde_json::to_string(&error).unwrap_or_else(|_| error.message.clone());
        match error.code {
            ErrorCode::InvalidRequest | ErrorCode::InvalidField => Self::InvalidRequest(document),
            ErrorCode::Registry | ErrorCode::ImageNotFound => Self::Unavailable(document),
            ErrorCode::Repart | ErrorCode::Internal => Self::Failed(document),
            _ => Self::Refused(document),
        }
    }
}

fn failed(error: anyhow::Error) -> Error {
    Error::from(PlanError::new(
        ErrorCode::Internal,
        None,
        format!("{error:#}"),
    ))
}

struct Installer {
    helper: Arc<Helper>,
}

#[interface(name = "org.ilex.Installer1")]
impl Installer {
    /// This computer, its disks and what the installer offers, as JSON.
    async fn probe(&self) -> Result<String, Error> {
        let helper = Arc::clone(&self.helper);
        let probe = blocking::unblock(move || helper.probe())
            .await
            .map_err(failed)?;
        serde_json::to_string(&probe).map_err(|e| failed(e.into()))
    }

    /// Check a request and return the plan; nothing is written.
    async fn plan(&self, request: String) -> Result<String, Error> {
        let helper = Arc::clone(&self.helper);
        let plan = blocking::unblock(move || helper.plan(&request)).await?;
        serde_json::to_string(&plan).map_err(|e| failed(e.into()))
    }

    /// Where an installation is; a restarted interface re-attaches with this.
    async fn state(&self) -> Result<String, Error> {
        serde_json::to_string(&self.helper.state()).map_err(|e| failed(e.into()))
    }
}

/// Own the name and serve until the process is stopped.
pub fn serve(helper: Arc<Helper>) -> Result<()> {
    zbus::block_on(async move {
        let _connection = connection::Builder::system()
            .name(NAME)
            .serve_at(PATH, Installer { helper })
            .build()
            .await?;
        std::future::pending::<()>().await;
        Ok(())
    })
}
