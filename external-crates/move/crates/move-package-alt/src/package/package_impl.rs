// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, path::Path};

use super::manifest::Manifest;
use super::paths::PackagePath;
use crate::{
    compatibility::{
        legacy::LegacyData,
        legacy_parser::{is_legacy_like, parse_legacy_manifest_from_file},
    },
    dependency::{PinnedDependencyInfo, pin},
    errors::{PackageError, PackageResult},
    flavor::MoveFlavor,
    package::lockfile::Lockfiles,
    schema::{LocalDepInfo, LockfileDependencyInfo, OriginalID, Publication, PublishedID},
};
use move_core_types::identifier::Identifier;

pub type EnvironmentName = String;
pub type EnvironmentID = String;

pub type PackageName = Identifier;
pub type AddressInfo = String;

#[derive(Debug)]
pub struct Package<F: MoveFlavor> {
    // TODO: maybe hold a lock on the lock file? Maybe not if move-analyzer wants to hold on to a
    // Package long term?
    manifest: Manifest<F>,
    /// A [`PackagePath`] representing the canonical path to the package directory.
    path: PackagePath,
    /// The on-chain publish information per environment
    /// TODO(manos): Replace this with a type as it's used in many places.
    publish_data: BTreeMap<EnvironmentName, Publication<F>>,
    /// The way this package should be serialized to the lockfile
    source: LockfileDependencyInfo,
    /// Optional legacy information for a supplied package.
    pub legacy_data: Option<LegacyData>,
}

impl<F: MoveFlavor> Package<F> {
    /// Load a package from the manifest.
    /// Makes a best effort to translate old-style packages into the current format,
    ///
    /// Fails if [path] does not exist, or if it doesn't contain a manifest
    pub async fn load_root(path: impl AsRef<Path>) -> PackageResult<Self> {
        let path = PackagePath::new(path.as_ref().to_path_buf())?;

        Self::load_internal(
            path,
            LockfileDependencyInfo::Local(LocalDepInfo { local: ".".into() }),
        )
        .await
    }

    /// Fetch [dep] and load a package from the fetched source
    /// Makes a best effort to translate old-style packages into the current format,
    pub async fn load(dep: PinnedDependencyInfo) -> PackageResult<Self> {
        let path = PackagePath::new(dep.fetch().await?)?;

        Self::load_internal(path, dep.into()).await
    }

    /// Loads a package internally, doing a "best" effort to translate an old-style package into the new one.
    async fn load_internal(
        path: PackagePath,
        source: LockfileDependencyInfo,
    ) -> PackageResult<Self> {
        let manifest = Manifest::<F>::read_from_file(path.manifest_path());

        // If our "modern" manifest is OK, we load the modern lockfile and return early.
        if let Ok(manifest) = manifest {
            let publish_data = Self::load_published_info_from_lockfile(&path)?;
            return Ok(Self {
                manifest,
                path,
                publish_data,
                source,
                legacy_data: None,
            });
        }

        // If the manifest does not look like a legacy one, we again return early by erroring on the modern errors.
        if !is_legacy_like(&path) {
            return Err(PackageError::Manifest(manifest.unwrap_err()));
        }

        // Here, that means that we're working on legacy package, so we can throw its errors.
        let legacy_manifest = parse_legacy_manifest_from_file(&path)?;

        Ok(Self {
            manifest: Manifest::from_parsed_manifest(
                legacy_manifest.parsed_manifest,
                legacy_manifest.file_handle,
            )?,
            path,
            publish_data: Default::default(),
            source,
            legacy_data: Some(legacy_manifest.legacy_data),
        })
    }

    /// Try to load a lockfile and extract the published information for each environment from it
    fn load_published_info_from_lockfile(
        path: &PackagePath,
    ) -> PackageResult<BTreeMap<EnvironmentName, Publication<F>>> {
        let lockfile = Lockfiles::<F>::read_from_dir(path)?;

        let publish_data = lockfile
            .map(|l| l.published().clone())
            .map(|x| {
                x.into_iter()
                    .map(|(env, pub_info)| (pub_info.chain_id.clone(), pub_info))
                    .collect()
            })
            .unwrap_or_default();

        Ok(publish_data)
    }

    /// The path to the root directory of this package. This path is guaranteed to exist
    /// and contain a manifest file.
    pub fn path(&self) -> &PackagePath {
        &self.path
    }

    pub fn name(&self) -> &PackageName {
        self.manifest().package_name()
    }

    /// TODO: comment
    pub fn manifest(&self) -> &Manifest<F> {
        &self.manifest
    }

    pub fn dep_for_self(&self) -> &LockfileDependencyInfo {
        &self.source
    }

    pub fn is_legacy(&self) -> bool {
        self.legacy_data.is_some()
    }

    /// The resolved and pinned dependencies from the manifest for environment `env`
    pub async fn direct_deps(
        &self,
        env: &EnvironmentName,
    ) -> PackageResult<BTreeMap<PackageName, PinnedDependencyInfo>> {
        let mut deps = self.manifest.dependencies();

        // TODO: This will probably go away after our discussions.
        if self.manifest().environments().get(env).is_none() && self.legacy_data.is_none() {
            return Err(PackageError::Generic(format!(
                "Package {} does not have `{env}` defined as an environment in its manifest",
                self.name()
            )));
        }

        let envs: BTreeMap<_, _> = self
            .manifest()
            .environments()
            .iter()
            .filter(|(e, _)| *e == env)
            .map(|(env, id)| (env.clone(), id.clone()))
            .collect();
        let pinned_deps = pin::<F>(deps.clone(), &envs).await?;

        Ok(pinned_deps
            .into_iter()
            .map(|(_, id, dep)| (id, dep))
            .collect())
    }

    /// Tries to get the `published-at` entry for the given package,
    /// including support for backwards compatibility (legacy packages)
    pub fn published_at(&self, env: &EnvironmentName) -> PackageResult<PublishedID> {
        if let Some(publish_data) = self.publish_data.get(env) {
            return Ok(publish_data.published_at.clone());
        }

        self.legacy_data
            .as_ref()
            .and_then(|d| d.published_at(env))
            .ok_or(PackageError::Generic(format!(
                "Package {} does not have a `published-at` ID for environment: `{env}`",
                self.name()
            )))
    }

    /// Tries to get the `original-id` entry for the given package,
    /// including support for backwards compatibility (legacy packages)
    pub fn original_id(&self, env: &EnvironmentName) -> PackageResult<OriginalID> {
        if let Some(publish_data) = self.publish_data.get(env) {
            return Ok(publish_data.original_id.clone());
        }

        self.legacy_data
            .as_ref()
            .and_then(|d| d.original_id(env))
            .ok_or(PackageError::Generic(format!(
                "Package {} does not have an `original-id` for environment: `{env}`",
                self.name()
            )))
    }
}
