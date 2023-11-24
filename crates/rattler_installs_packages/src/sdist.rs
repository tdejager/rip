use crate::core_metadata::WheelCoreMetadata;
use crate::utils::ReadAndSeek;
use crate::{Artifact, NormalizedPackageName, SDistFilename, SDistFormat};
use flate2::read::GzDecoder;
use miette::{miette, IntoDiagnostic};
use parking_lot::Mutex;
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use tar::Archive;

/// Represents a source distribution artifact.
pub struct SDist {
    /// Name of the source distribution
    name: SDistFilename,

    /// Source dist archive
    archive: Mutex<Archive<Box<dyn Read + Send>>>,
}

impl SDist {
    /// Create this struct from a path
    #[allow(dead_code)]
    pub fn from_path(
        path: &Path,
        normalized_package_name: &NormalizedPackageName,
    ) -> miette::Result<Self> {
        let file_name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| miette::miette!("path does not contain a filename"))?;
        let name =
            SDistFilename::from_filename(file_name, normalized_package_name).into_diagnostic()?;
        let bytes = std::fs::File::open(path).into_diagnostic()?;
        Self::new(name, Box::new(bytes))
    }

    /// Find entry in tar archive
    fn find_entry(&self, name: impl AsRef<str>) -> miette::Result<Option<Vec<u8>>> {
        let mut archive = self.archive.lock();
        // Loop over entries
        for entry in archive.entries().into_diagnostic()? {
            let mut entry = entry.into_diagnostic()?;

            // Find name in archive and return this
            if entry.path().into_diagnostic()?.ends_with(name.as_ref()) {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).into_diagnostic()?;
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    /// Read .PKG-INFO from the archive
    pub fn read_package_info(&self) -> miette::Result<(Vec<u8>, WheelCoreMetadata)> {
        if let Some(bytes) = self.find_entry("PKG-INFO")? {
            let metadata = WheelCoreMetadata::try_from(bytes.as_slice()).into_diagnostic()?;

            Ok((bytes, metadata))
        } else {
            Err(miette!("no PKG-INFO found in archive"))
        }
    }

    /// Read the build system info from the pyproject.toml
    #[allow(dead_code)]
    pub fn read_build_info(&self) -> miette::Result<pyproject_toml::BuildSystem> {
        if let Some(bytes) = self.find_entry("pyproject.toml")? {
            let source = String::from_utf8(bytes).into_diagnostic()?;
            let project = pyproject_toml::PyProjectToml::new(&source).into_diagnostic()?;
            Ok(project
                .build_system
                .ok_or_else(|| miette!("no build-system found in pyproject.toml"))?)
        } else {
            Err(miette!("no pyproject.toml found in archive"))
        }
    }

    /// Checks if this artifact implements PEP 643
    /// and returns the metadata if it does
    pub fn pep643_metadata(&self) -> Option<(Vec<u8>, WheelCoreMetadata)> {
        // Assume we have a PKG-INFO
        let (bytes, metadata) = self.read_package_info().ok()?;
        if metadata.metadata_version.implements_pep643() {
            Some((bytes, metadata))
        } else {
            None
        }
    }
}

impl Artifact for SDist {
    type Name = SDistFilename;

    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self> {
        let sdist = match name.format {
            SDistFormat::TarGz => {
                let bytes = Box::new(GzDecoder::new(bytes));
                Self {
                    name,
                    archive: Mutex::new(Archive::new(bytes)),
                }
            }
            SDistFormat::Tar => {
                let bytes: Box<dyn Read + Send> = Box::new(bytes);
                Self {
                    name,
                    archive: Mutex::new(Archive::new(bytes)),
                }
            }
            _ => return Err(miette!("unsupported format {:?}", name.format)),
        };
        Ok(sdist)
    }

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use crate::sdist::SDist;
    use insta::assert_ron_snapshot;
    use std::path::Path;

    #[test]
    pub fn reject_rich_metadata() {
        // Read path
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        // Load sdist
        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        // Rich has an old metadata version
        let metadata = sdist.pep643_metadata();
        assert!(metadata.is_none());
    }

    #[test]
    pub fn correct_metadata_fake_flask() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/fake-flask-3.0.0.tar.gz");

        let sdist = SDist::from_path(&path, &"fake-flask".parse().unwrap()).unwrap();
        // Should not fail as it is a valid PKG-INFO
        // and considered reliable
        sdist.pep643_metadata().unwrap();
    }

    #[test]
    pub fn read_rich_build_info() {
        // Read path
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        // Load sdist
        let sdist = super::SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let build_system = sdist.read_build_info().unwrap();

        assert_ron_snapshot!(build_system, @r###"
        BuildSystem(
          requires: [
            "poetry-core >=1.0.0",
          ],
          r#build-backend: Some("poetry.core.masonry.api"),
          r#backend-path: None,
        )
        "###);
    }
}
