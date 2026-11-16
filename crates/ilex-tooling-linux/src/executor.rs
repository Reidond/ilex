//! Synchronous local Linux executor. The CLI owns cancellation/process lifetime;
//! remote staging/cancellation is deliberately not claimed by this adapter.
use crate::{Workspace, cmd};
use anyhow::{Context, Result, bail, ensure};
use ilex_tooling_core::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub const RECIPE_VERSION: &str = "linux-image-v1";
pub struct LocalBuildExecutor {
    ws: Workspace,
    identity: CheckoutIdentity,
    id: String,
}
impl LocalBuildExecutor {
    pub fn new(ws: Workspace) -> Result<Self> {
        crate::require_linux()?;
        let identity = ws.identity()?;
        let machine = fs::read_to_string("/etc/machine-id")
            .context("identifying the local Linux executor")?;
        let store =
            cmd::output(Command::new("podman").args(["info", "--format", "{{.Store.GraphRoot}}"]))
                .context("identifying the local Podman store")?;
        let id = format!(
            "linux-local:{}",
            digest(format!("{}\0{store}", machine.trim()).as_bytes())
        );
        Ok(Self { ws, identity, id })
    }
    fn executor_id(&self) -> String {
        self.id.clone()
    }
    fn job_dir(&self, job: &str) -> PathBuf {
        self.ws
            .state_dir()
            .join("jobs")
            .join(&digest(job.as_bytes())[7..])
    }
    fn authorize(&self, request: &BuildRequest) -> Result<()> {
        check_version(request.schema)?;
        request.source.validate()?;
        self.identity.authorize(&request.checkout.owner)?;
        ensure!(
            request.checkout.checkout_id == self.identity.checkout_id
                && request.checkout.repository_id == self.identity.repository_id,
            "checkout identity mismatch"
        );
        ensure!(
            request.checkout.slug == self.identity.slug
                && request.checkout.is_main == self.identity.is_main,
            "checkout namespace mismatch"
        );
        let revision = if request.source.dirty {
            format!("{}-dirty", request.source.revision)
        } else {
            request.source.revision.clone()
        };
        ensure!(
            request.checkout.source_revision == revision,
            "checkout/source revision mismatch"
        );
        ensure!(!request.job.is_empty(), "job identity is empty");
        ensure!(
            request.recipe_version == RECIPE_VERSION,
            "unsupported local recipe version"
        );
        ensure!(
            request.environment.is_empty(),
            "local executor does not accept environment overrides"
        );
        ensure!(
            request.profile.is_none(),
            "local executor has no profile selection yet"
        );
        crate::provider::require_arch(request.guest_arch)?;
        ensure!(
            request.host_arch == Architecture::host()?,
            "executor host architecture mismatch"
        );
        Ok(())
    }
}
impl LocalBuildExecutor {
    pub fn execute_rebuilding_legacy(
        &self,
        request: &BuildRequest,
        expected_id: &str,
    ) -> Result<BuildResult> {
        self.execute_inner(request, Some(expected_id))
    }
    fn execute_inner(
        &self,
        request: &BuildRequest,
        legacy_expected: Option<&str>,
    ) -> Result<BuildResult> {
        self.stage(request)?;
        let BuildOperation::Image(options) = &request.operation else {
            bail!(
                "this local contract executor implements image builds; use the compatibility disk/ISO/tool commands for other operations"
            );
        };
        crate::ownership::validate_build_args(&options.podman_args)?;
        let transaction = match legacy_expected {
            Some(id) => {
                crate::ownership::ImageTransaction::rebuild_legacy(&self.ws, &options.output, id)?
            }
            None => crate::ownership::ImageTransaction::new(&self.ws, &options.output)?,
        };
        // Waiting for another writer can outlive the staged source inventory.
        self.stage(request)?;
        let probe = self.probe()?;
        let dir = self.job_dir(&request.job);
        fs::create_dir_all(&dir)?;
        // Request is written only after validating schema, ownership and bytes.
        fs::write(
            dir.join("request.json"),
            serde_json::to_vec_pretty(request)?,
        )?;
        let mut temporary = options.clone();
        temporary.output = transaction.temporary().into();
        let image = crate::image::build(&self.ws, request, &temporary)?;
        let after = crate::source::manifest(&self.ws)?;
        ensure!(
            after.digest == request.source.digest && after.revision == request.source.revision,
            "source changed while Podman was building; result has not been accepted; rebuild from stable input"
        );
        let immutable_id = crate::bcvk::image_id(&image)?;
        let registry_digest = Some(crate::image::image_digest(&image)?).filter(|s| !s.is_empty());
        let image = transaction.promote(&self.ws)?;
        let result = BuildResult {
            schema: SCHEMA_VERSION,
            job: request.job.clone(),
            owner: self.identity.owner.clone(),
            source_digest: request.source.digest.clone(),
            image: Some(ImageIdentity {
                executor_id: probe.executor_id.clone(),
                local_reference: image.clone(),
                immutable_id,
                registry_digest,
                architecture: request.guest_arch,
            }),
            executor_id: probe.executor_id,
            versions: probe.versions,
            logs: vec![],
            artifacts: vec![],
        };
        fs::write(dir.join("result.json"), serde_json::to_vec_pretty(&result)?)?;
        Ok(result)
    }
}
impl BuildExecutor for LocalBuildExecutor {
    fn probe(&self) -> Result<ExecutorProbe> {
        crate::provider::require_arch(Architecture::X86_64)?;
        let mut versions = BTreeMap::new();
        versions.insert(
            "podman".into(),
            cmd::output(Command::new("podman").arg("--version"))?,
        );
        Ok(ExecutorProbe {
            executor_id: self.executor_id(),
            host_arch: Architecture::host()?,
            supported_guest_arches: vec![Architecture::X86_64],
            versions,
        })
    }
    fn stage(&self, request: &BuildRequest) -> Result<String> {
        self.authorize(request)?;
        let source = crate::source::manifest(&self.ws)?;
        ensure!(
            source.digest == request.source.digest && source.revision == request.source.revision,
            "local checkout differs from requested source manifest; retry build"
        );
        ensure!(
            request.checkout.snapshot_digest.as_deref() == Some(source.digest.as_str()),
            "checkout source digest mismatch"
        );
        Ok(source.digest)
    }
    fn execute(&self, request: &BuildRequest) -> Result<BuildResult> {
        self.execute_inner(request, None)
    }
    fn inspect(&self, job: &str) -> Result<BuildResult> {
        self.ws.ensure_state()?;
        let result: BuildResult =
            serde_json::from_slice(&fs::read(self.job_dir(job).join("result.json"))?)?;
        check_version(result.schema)?;
        self.identity.authorize(&result.owner)?;
        ensure!(
            result.job == job && result.executor_id == self.executor_id(),
            "job/executor identity mismatch"
        );
        Ok(result)
    }
    fn export(&self, image: &ImageIdentity, output: &Path) -> Result<ArtifactLocation> {
        crate::require_linux()?;
        self.ws.ensure_state()?;
        crate::ownership::require_image(&self.ws, &image.immutable_id)?;
        ensure!(
            image.executor_id == self.executor_id(),
            "image belongs to a different executor"
        );
        let exports = self.ws.state_dir().join("exports");
        fs::create_dir_all(&exports)?;
        let name = output.file_name().context("export filename is missing")?;
        let owned = exports.join(name);
        cmd::run(
            Command::new("podman")
                .args(["save", "--output"])
                .arg(&owned)
                .arg(&image.immutable_id),
        )?;
        let artifact = ArtifactLocation {
            executor_id: self.executor_id(),
            path: owned.clone(),
            sha256: digest(&fs::read(&owned)?),
        };
        if std::path::absolute(output)? != std::path::absolute(&owned)? {
            self.fetch(&artifact, output)?;
        }
        Ok(artifact)
    }
    fn fetch(&self, artifact: &ArtifactLocation, output: &Path) -> Result<()> {
        self.ws.ensure_state()?;
        ensure!(
            artifact.executor_id == self.executor_id(),
            "artifact belongs to a different executor"
        );
        crate::provider::require_owned_path(&self.ws.state_dir(), &artifact.path)?;
        let bytes = fs::read(&artifact.path)?;
        ensure!(
            digest(&bytes) == artifact.sha256,
            "artifact checksum mismatch"
        );
        fs::write(output, bytes)?;
        Ok(())
    }
    fn cancel(&self, _: &str) -> Result<()> {
        bail!(
            "local synchronous builds are cancelled by their owning CLI process; detached remote jobs are not supported"
        )
    }
}
