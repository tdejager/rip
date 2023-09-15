mod writer;

use crate::writer::{global_multi_progress, IndicatifWriter};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use itertools::Itertools;
use miette::{Context, IntoDiagnostic};
use rattler_installs_packages::requirement::Requirement;
use rattler_installs_packages::{
    NormalizedPackageName, PackageDb, PackageName, PackageRequirement, Specifiers, Version, Wheel,
};
use rattler_libsolv_rs::{
    Candidates, DefaultSolvableDisplay, Dependencies, DependencyProvider, NameId, Pool, SolvableId,
    Solver, VersionSet,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::{Debug, Display, Formatter};
use std::io::Write;
use std::time::Duration;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::util::SubscriberInitExt;
use url::Url;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(num_args=1.., required=true)]
    specs: Vec<PackageRequirement>,

    /// Base URL of the Python Package Index (default https://pypi.org/simple). This should point
    /// to a repository compliant with PEP 503 (the simple repository API).
    #[clap(default_value = "https://pypi.org/simple/", long)]
    index_url: Url,
}


#[repr(transparent)]
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct PypiVersionSet(Specifiers);

impl From<Specifiers> for PypiVersionSet {
    fn from(value: Specifiers) -> Self {
        Self(value)
    }
}

impl Display for PypiVersionSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
struct PypiVersion(Version);

impl VersionSet for PypiVersionSet {
    type V = PypiVersion;

    fn contains(&self, v: &Self::V) -> bool {
        match self.0.satisfied_by(&v.0) {
            Err(e) => {
                tracing::error!("failed to determine if '{}' contains '{}': {e}", &self.0, v);
                false
            }
            Ok(result) => result,
        }
    }
}

impl Display for PypiVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0)
    }
}

struct PypiDependencyProvider {
    pool: Pool<PypiVersionSet, NormalizedPackageName>,
    candidates: HashMap<NameId, Candidates>,
    dependencies: HashMap<SolvableId, Dependencies>,
}

impl DependencyProvider<PypiVersionSet, NormalizedPackageName> for PypiDependencyProvider {
    fn pool(&self) -> &Pool<PypiVersionSet, NormalizedPackageName> {
        &self.pool
    }

    fn sort_candidates(
        &self,
        solver: &Solver<PypiVersionSet, NormalizedPackageName, Self>,
        solvables: &mut [SolvableId],
    ) {
        solvables.sort_by(|&a, &b| {
            let solvable_a = solver.pool().resolve_solvable(a);
            let solvable_b = solver.pool().resolve_solvable(b);

            let a = &solvable_a.inner().0;
            let b = &solvable_b.inner().0;

            // Sort in reverse order from highest to lowest.
            b.cmp(a)
        })
    }

    fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        self.candidates.get(&name).cloned()
    }

    fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        self.dependencies.get(&solvable).cloned().unwrap_or_default()
    }
}

/// Download all metadata needed to solve the specified packages.
async fn recursively_get_metadata(
    package_db: &PackageDb,
    packages: Vec<PackageName>,
    multi_progress: MultiProgress,
) -> miette::Result<PypiDependencyProvider> {
    let mut queue = VecDeque::from_iter(packages.into_iter());
    let mut seen = HashSet::<PackageName>::from_iter(queue.iter().cloned());

    let progress_bar = multi_progress.add(ProgressBar::new(0));
    progress_bar.set_style(
        ProgressStyle::with_template("{spinner:.green} fetching metadata ({pos}/{len}) {wide_msg}")
            .unwrap(),
    );
    progress_bar.enable_steady_tick(Duration::from_millis(100));

    // TODO: https://peps.python.org/pep-0508/#environment-markers
    let env = HashMap::from_iter([
        // TODO: We should add some proper values here.
        // See: https://peps.python.org/pep-0508/#environment-markers
        ("os_name", ""),
        ("sys_platform", ""),
        ("platform_machine", ""),
        ("platform_python_implementation", ""),
        ("platform_release", ""),
        ("platform_system", ""),
        ("platform_version", ""),
        ("python_version", "3.9"),
        ("python_full_version", ""),
        ("implementation_name", ""),
        ("implementation_version", ""),
        // TODO: Add support for extras
        ("extra", ""),
    ]);

    let pool = Pool::new();
    let mut candidates: HashMap<_, Candidates> = HashMap::new();
    let mut dependencies: HashMap<_, Dependencies> = HashMap::new();

    progress_bar.set_length(seen.len() as u64);

    while let Some(package) = queue.pop_front() {
        tracing::info!("Fetching metadata for {}", package.as_str());

        let package_name_id =
            pool.intern_package_name::<NormalizedPackageName>(package.clone().into());

        // Get all the metadata for this package
        let artifacts = match package_db.available_artifacts(&package).await {
            Ok(artifacts) => artifacts,
            Err(err) => {
                tracing::error!(
                    "failed to fetch artifacts of '{}': {err:?}, skipping..",
                    package.as_str()
                );
                continue;
            }
        };

        let mut num_solvables = 0;

        // Fetch metadata per version
        for (version, artifacts) in artifacts.iter() {
            // Filter only artifacts we can work with
            let available_artifacts = artifacts
                .iter()
                // We are only interested in wheels
                .filter(|a| a.is::<Wheel>())
                // TODO: How to filter prereleases correctly?
                .filter(|a| {
                    a.filename.version().pre.is_none() && a.filename.version().dev.is_none()
                })
                .collect::<Vec<_>>();

            // Check if there are wheel artifacts for this version
            if available_artifacts.is_empty() {
                // If there are no wheel artifacts, we're just gonna skip it
                tracing::warn!(
                    "No available wheel artifact {} {version} (skipping)",
                    package.as_str()
                );
                continue;
            }

            // Filter yanked artifacts
            let non_yanked_artifacts = artifacts
                .iter()
                .filter(|a| !a.yanked.yanked)
                .collect::<Vec<_>>();

            if non_yanked_artifacts.is_empty() {
                tracing::info!("{} {version} was yanked (skipping)", package.as_str());
                continue;
            }

            let (_, metadata) = package_db
                .get_metadata::<Wheel, _>(artifacts)
                .await
                .with_context(|| {
                    format!(
                        "failed to download metadata for {} {version}",
                        package.as_str(),
                    )
                })?;

            // let solvable_id = pool.add_package(package_name_id, PypiVersion(version.clone()));
            let solvable_id = pool.intern_solvable(package_name_id, PypiVersion(version.clone()));
            candidates.entry(package_name_id).or_default().candidates.push(solvable_id);

            // Iterate over all requirements and add them to the queue if we don't have information on them yet.
            for requirement in metadata.requires_dist {
                // Evaluate environment markers
                if let Some(env_marker) = &requirement.env_marker_expr {
                    if !env_marker.eval(&env)? {
                        // tracing::info!("skipping dependency {requirement}");
                        continue;
                    }
                }

                // Add the package if we didnt see it yet.
                if !seen.contains(&requirement.name) {
                    println!(
                        "adding {} from requirement: {requirement}",
                        requirement.name.as_str()
                    );
                    queue.push_back(requirement.name.clone());
                    seen.insert(requirement.name.clone());
                }

                // Add the dependency to the pool
                let Requirement {
                    name, specifiers, ..
                } = requirement.into_inner();
                let dependency_name_id = pool.intern_package_name(name);
                let version_set_id = pool.intern_version_set(dependency_name_id, specifiers.into());
                dependencies
                    .entry(solvable_id)
                    .or_default()
                    .requirements
                    .push(version_set_id);
                // pool.add_dependency(solvable_id, version_set_id);
            }

            num_solvables += 1;
        }

        if num_solvables == 0 {
            tracing::error!(
                "could not find any suitable artifact for {}, does the package provide any wheels?",
                package.as_str()
            );
        }

        progress_bar.set_length(seen.len() as u64);
        progress_bar.set_position(seen.len().saturating_sub(queue.len()) as u64);
        progress_bar.set_message(format!(
            "{}..",
            queue
                .iter()
                .take(10)
                .format_with(",", |p, f| f(&p.as_str()))
        ))
    }

    Ok(PypiDependencyProvider { pool, candidates, dependencies })
}

async fn actual_main() -> miette::Result<()> {
    let args = Args::parse();

    // Setup tracing subscriber
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_span_events(FmtSpan::ENTER)
        .with_writer(IndicatifWriter::new(global_multi_progress()))
        .finish()
        .init();

    // Determine cache directory
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| miette::miette!("failed to determine cache directory"))?
        .join("rattler/pypi");
    tracing::info!("cache directory: {}", cache_dir.display());

    // Construct a package database
    let package_db = rattler_installs_packages::PackageDb::new(
        Default::default(),
        &[normalize_index_url(args.index_url)],
        cache_dir.clone(),
    )
    .into_diagnostic()?;

    // Get metadata for all the packages
    let provider = recursively_get_metadata(
        &package_db,
        args.specs.iter().map(|spec| spec.name.clone()).collect(),
        global_multi_progress(),
    )
    .await?;

    // Create a task to solve the specs passed on the command line.
    let mut root_requirements = Vec::with_capacity(args.specs.len());
    for Requirement {
        name, specifiers, ..
    } in args.specs.iter().map(PackageRequirement::as_inner)
    {
        let dependency_package_name = provider.pool().intern_package_name(name.clone());
        let version_set_id = provider
            .pool()
            .intern_version_set(dependency_package_name, specifiers.clone().into());
        root_requirements.push(version_set_id);
    }

    // Solve the jobs
    let mut solver = Solver::new(provider);
    let result = solver.solve(root_requirements);
    let artifacts = match result {
        Err(e) => {
            eprintln!(
                "Could not solve:\n{}",
                e.display_user_friendly(&solver, &DefaultSolvableDisplay)
            );
            return Ok(());
        }
        Ok(transaction) => transaction
            .into_iter()
            .map(|result| {
                let pool = solver.pool();
                let solvable = pool.resolve_solvable(result);
                let name = pool.resolve_package_name(solvable.name_id());
                (name.clone(), solvable.inner().0.clone())
            })
            .collect::<Vec<_>>(),
    };

    // Output the selected versions
    println!("{}:", console::style("Resolved environment").bold());
    for spec in args.specs.iter() {
        println!("- {}", spec);
    }

    println!();
    let mut tabbed_stdout = tabwriter::TabWriter::new(std::io::stdout());
    writeln!(
        tabbed_stdout,
        "{}\t{}",
        console::style("Name").bold(),
        console::style("Version").bold()
    )
    .into_diagnostic()?;
    for (name, artifact) in artifacts {
        writeln!(tabbed_stdout, "{name}\t{artifact}").into_diagnostic()?;
    }
    tabbed_stdout.flush().unwrap();

    Ok(())
}


#[tokio::main]
async fn main() {
    if let Err(e) = actual_main().await {
        eprintln!("{e:?}");
    }
}


fn normalize_index_url(mut url: Url) -> Url {
    let path = url.path();
    if !path.ends_with('/') {
        url.set_path(&format!("{path}/"));
    }
    url
}

#[cfg(test)]
mod test {
    use rattler_installs_packages::Version;

    #[test]
    fn valid_version() {
        assert!(Version::parse("2011k").is_some());
    }
}
