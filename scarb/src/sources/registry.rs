use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use tracing::trace;

use scarb_ui::components::Status;

use crate::core::registry::client::RegistryClient;
use crate::core::registry::client::cache::RegistryClientCache;
use crate::core::registry::client::http::HttpRegistryClient;
use crate::core::registry::client::local::LocalRegistryClient;
use crate::core::registry::index::IndexRecord;
use crate::core::registry::package_source_store::PackageSourceStore;
use crate::core::source::Source;
use crate::core::{
    Checksum, Config, DependencyVersionReq, ManifestDependency, Package, PackageId, SourceId,
    Summary,
};
use crate::flock::FileLockGuard;
use crate::sources::PathSource;
use std::collections::HashSet;

pub struct RegistrySource<'c> {
    source_id: SourceId,
    config: &'c Config,
    client: RegistryClientCache<'c>,
    package_sources: PackageSourceStore<'c>,
    yanked_whitelist: HashSet<PackageId>,
}

impl<'c> RegistrySource<'c> {
    pub fn new(
        source_id: SourceId,
        config: &'c Config,
        yanked_whitelist: &HashSet<PackageId>,
    ) -> Result<Self> {
        let client = Self::create_client(source_id, config)?;
        let client = RegistryClientCache::new(source_id, client, config)?;

        let package_sources = PackageSourceStore::new(source_id, config);

        Ok(Self {
            source_id,
            config,
            client,
            package_sources,
            yanked_whitelist: yanked_whitelist.clone(),
        })
    }

    pub fn create_client(
        source_id: SourceId,
        config: &'c Config,
    ) -> Result<Box<dyn RegistryClient + 'c>> {
        assert!(source_id.is_registry());
        match source_id.url.scheme() {
            "file" => {
                trace!("creating local registry client for: {source_id}");
                let path = source_id
                    .url
                    .to_file_path()
                    .map_err(|_| anyhow!("url is not a valid path: {}", source_id.url))?;
                Ok(Box::new(LocalRegistryClient::new(&path, config)?))
            }
            "http" | "https" => {
                trace!("creating http registry client for: {source_id}");
                Ok(Box::new(HttpRegistryClient::new(source_id, config)?))
            }
            _ => {
                bail!("unsupported registry protocol: {source_id}")
            }
        }
    }
}

#[async_trait]
impl Source for RegistrySource<'_> {
    #[tracing::instrument(level = "trace", skip(self))]
    async fn query(&self, dependency: &ManifestDependency) -> Result<Vec<Summary>> {
        let records = self
            .client
            .get_records_with_cache(dependency)
            .await
            .with_context(|| {
                format!(
                    "failed to lookup for `{dependency}` in registry: {}",
                    self.source_id
                )
            })?;

        let build_summary_from_index_record = |record: &IndexRecord| {
            let package_id = PackageId::new(
                dependency.name.clone(),
                record.version.clone(),
                self.source_id,
            );

            if record.yanked && !self.yanked_whitelist.contains(&package_id) {
                return None;
            }
            let dependencies = record
                .dependencies
                .iter()
                .map(|index_dep| {
                    ManifestDependency::builder()
                        .name(index_dep.name.clone())
                        .version_req(DependencyVersionReq::from(index_dep.req.clone()))
                        .source_id(self.source_id)
                        .build()
                })
                .collect();

            Some(
                Summary::builder()
                    .package_id(package_id)
                    .dependencies(dependencies)
                    .no_core(record.no_core)
                    .checksum(Some(record.checksum.clone()))
                    .build(),
            )
        };
        Ok(records
            .iter()
            // NOTE: We filter based on IndexRecords here, to avoid unnecessarily allocating
            //   PackageIds just to abandon them soon after.
            // NOTE: Technically, RegistryClientCache may already have filtered the records,
            //   but it is not required to do so, so we do it here again as a safety measure.
            .filter_map(|record| {
                if dependency.version_req.matches(&record.version) {
                    build_summary_from_index_record(record)
                } else {
                    None
                }
            })
            .collect())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn download(&self, id: PackageId) -> Result<Package> {
        let (archive, checksum) = self
            .client
            .download_and_verify_with_cache(id)
            .await
            .with_context(|| format!("failed to download package: {id}"))?;

        self.load_package(id, archive, checksum).await
    }
}

impl RegistrySource<'_> {
    /// Turn the downloaded `.tar.zst` tarball into a [`Package`].
    ///
    /// This method extracts the tarball into cache directory, and then loads it using
    /// suitably configured [`PathSource`].
    async fn load_package(
        &self,
        id: PackageId,
        archive: FileLockGuard,
        checksum: Checksum,
    ) -> Result<Package> {
        self.config
            .ui()
            .verbose(Status::new("Unpacking", &id.to_string()));

        // `extract` drops the archive internally and thus handles file unlocking.
        let path = self.package_sources.extract(id, archive).await?;

        let path_source = PathSource::recursive_at(&path, self.source_id, self.config);
        let mut package = path_source.download(id).await?;

        package.manifest_mut().summary.set_checksum(checksum);

        Ok(package)
    }
}

impl fmt::Debug for RegistrySource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrySource")
            .field("source", &self.source_id.to_string())
            .finish_non_exhaustive()
    }
}
