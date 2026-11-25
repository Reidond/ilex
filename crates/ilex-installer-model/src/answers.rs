//! `answers.toml`: an unattended installation's questions, answered. It lives on
//! a volume labelled `ILEX-ANSWERS`, may hold plaintext secrets (the
//! documentation says so) and is read once by the helper. This module is the
//! schema and its syntax checks; reading the volume and applying the answers
//! belong to the installation itself.

use crate::plan::{ErrorCode, Mode, PlanError, PlanRequest, TpmMode};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fmt;

pub const ANSWERS_SCHEMA: u32 = 1;

/// A secret read from a file. It never prints.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answers {
    pub schema: u32,
    pub install: InstallAnswers,
    #[serde(default)]
    pub encryption: EncryptionAnswers,
    #[serde(default)]
    pub system: SystemAnswers,
    pub administrator: AdministratorAnswers,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct InstallAnswers {
    /// Defaults to the installer's own selection (boot override, then
    /// detection).
    pub composition: Option<String>,
    /// A disk's stable id.
    pub disk: String,
    pub mode: Mode,
    /// Whole-disk mode: the disk id again, as the destructive confirmation.
    pub confirm_erase: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct EncryptionAnswers {
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub tpm: Option<TpmMode>,
    /// The disk passphrase; required when encryption is on.
    pub passphrase: Option<Secret>,
}

fn enabled() -> bool {
    true
}

impl Default for EncryptionAnswers {
    fn default() -> Self {
        Self {
            enabled: true,
            tpm: None,
            passphrase: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SystemAnswers {
    pub keyboard: Option<String>,
    pub language: Option<String>,
    pub time_zone: Option<String>,
    pub host_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdministratorAnswers {
    pub name: String,
    pub password: Secret,
}

impl Answers {
    pub fn parse(text: &str) -> Result<Self> {
        let answers: Self = toml::from_str(text).context("reading answers.toml")?;
        anyhow::ensure!(
            answers.schema == ANSWERS_SCHEMA,
            "unsupported answers schema {}",
            answers.schema
        );
        Ok(answers)
    }

    /// The plan request these answers make. `selected` is the installer's own
    /// choice, used when the answers name no composition.
    pub fn to_request(&self, selected: &str) -> Result<PlanRequest, PlanError> {
        let invalid = |field: &str, message: &str| {
            Err(PlanError::new(
                ErrorCode::InvalidField,
                Some(field),
                message,
            ))
        };
        if self.install.mode == Mode::WholeDisk
            && self.install.confirm_erase.as_deref() != Some(self.install.disk.as_str())
        {
            return invalid(
                "install.confirm-erase",
                "whole-disk mode needs confirm-erase to repeat the disk id",
            );
        }
        if self.encryption.enabled
            && self
                .encryption
                .passphrase
                .as_ref()
                .is_none_or(|p| p.expose().is_empty())
        {
            return invalid(
                "encryption.passphrase",
                "encryption is on but no passphrase is given",
            );
        }
        if !self.encryption.enabled && self.encryption.passphrase.is_some() {
            return invalid(
                "encryption.passphrase",
                "a passphrase is given but encryption is off",
            );
        }
        if self.administrator.password.expose().is_empty() {
            return invalid(
                "administrator.password",
                "the administrator needs a password",
            );
        }
        let defaults = PlanRequest::parse(
            r#"{"composition":"x","disk":"x","mode":"whole-disk","administrator":"x"}"#,
        )?;
        let request = PlanRequest {
            composition: self
                .install
                .composition
                .clone()
                .unwrap_or_else(|| selected.to_owned()),
            disk: self.install.disk.clone(),
            mode: self.install.mode,
            encrypt: self.encryption.enabled,
            tpm: self.encryption.tpm,
            keyboard: self.system.keyboard.clone().unwrap_or(defaults.keyboard),
            language: self.system.language.clone().unwrap_or(defaults.language),
            time_zone: self.system.time_zone.clone().unwrap_or(defaults.time_zone),
            host_name: self.system.host_name.clone().unwrap_or(defaults.host_name),
            administrator: self.administrator.name.clone(),
        };
        request.check_syntax()?;
        Ok(request)
    }
}
