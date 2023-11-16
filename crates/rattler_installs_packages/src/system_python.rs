use std::path::PathBuf;
use thiserror::Error;

// TODO: remove this once we are using this for sdist creation
#[allow(dead_code)]

/// Error that can occur while finding the python executable.
#[derive(Debug, Error)]
pub enum FindPythonError {
    #[error("could not find python executable")]
    NotFound,
}

/// Try to find the python executable in the current environment.
pub fn system_python_executable() -> Result<PathBuf, FindPythonError> {
    // When installed with homebrew on macOS, the python3 executable is called `python3` instead
    // Also on some ubuntu installs this is the case
    // For windows it should just be python
    which::which("python3")
        .or_else(|_| which::which("python"))
        .map_err(|_| FindPythonError::NotFound)
}

/// Errors that can occur while trying to parse the python version
#[derive(Debug, Error)]
pub enum ParsePythonInterpreterVersionError {
    #[error("failed to parse version string, found '{0}' expect something like 'Python x.x.x'")]
    InvalidVersion(String),
    #[error(transparent)]
    FindPythonError(#[from] FindPythonError),
}

pub struct PythonInterpreterVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl PythonInterpreterVersion {
    /// Get the version of the python interpreter
    /// Expects the string from `python --version` as input
    /// getting something along the lines of `Python 3.8.5`
    pub fn from_python_output(
        version_str: &str,
    ) -> Result<Self, ParsePythonInterpreterVersionError> {
        use ParsePythonInterpreterVersionError::InvalidVersion;

        let mut parts = version_str.split_whitespace();
        if parts
            .next()
            .ok_or_else(|| InvalidVersion(version_str.to_owned()))?
            != "Python"
        {
            return Err(InvalidVersion(version_str.to_owned()));
        }
        let version = parts
            .next()
            .ok_or_else(|| InvalidVersion(version_str.to_owned()))?;
        let mut parts = version.split('.');

        let major = parts
            .next()
            .ok_or_else(|| InvalidVersion(version_str.to_owned()))?
            .parse()
            .map_err(|_| InvalidVersion(version_str.to_owned()))?;

        let minor = parts
            .next()
            .ok_or_else(|| InvalidVersion(version_str.to_owned()))?
            .parse()
            .map_err(|_| InvalidVersion(version_str.to_owned()))?;

        let patch = parts
            .next()
            .ok_or_else(|| InvalidVersion(version_str.to_owned()))?
            .parse()
            .map_err(|_| InvalidVersion(version_str.to_owned()))?;

        Ok(Self::new(major, minor, patch))
    }

    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Get the python version from the system interpreter
    pub fn from_system() -> Result<Self, ParsePythonInterpreterVersionError> {
        let output = std::process::Command::new(system_python_executable()?)
            .arg("--version")
            .output()
            .map_err(|_| FindPythonError::NotFound)?;
        let version_str = String::from_utf8_lossy(&output.stdout);
        Self::from_python_output(&version_str)
    }
}

#[cfg(test)]
mod tests {
    use crate::system_python::PythonInterpreterVersion;

    #[test]
    pub fn parse_python_version() {
        let version = PythonInterpreterVersion::from_python_output("Python 3.8.5").unwrap();
        assert_eq!(version.major, 3);
        assert_eq!(version.minor, 8);
        assert_eq!(version.patch, 5);
    }
}
