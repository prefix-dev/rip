use std::fmt;
use std::{
    fmt::{Display, Formatter},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
};

use fs_extra::dir::remove;
// use rattler_build::recipe::parser::{GitRev, GitSource, GitUrl};
// use rattler_build::source::git_source::git_src;
use rattler_digest::{compute_bytes_digest, parse_digest_from_hex, Sha256};
use serde::{Deserialize, Serialize};
use url::Url;

/// A Git repository URL or a local path to a Git repository
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GitUrl {
    /// A remote Git repository URL
    Url(Url),
    /// A local path to a Git repository
    Path(PathBuf),
}
impl Display for GitUrl {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            GitUrl::Url(url) => write!(f, "{url}"),
            GitUrl::Path(path) => write!(f, "{path:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// A git revision (branch, tag or commit)
pub enum GitRev {
    /// A git branch
    Branch(String),
    /// A git tag
    Tag(String),
    /// A specific git commit hash
    Commit(String),
    /// The default revision (HEAD)
    Head,
}

impl GitRev {
    /// Returns true if the revision is HEAD.
    pub fn is_head(&self) -> bool {
        matches!(self, Self::Head)
    }
}

impl ToString for GitRev {
    fn to_string(&self) -> String {
        match self {
            Self::Branch(branch) => format!("refs/heads/{}", branch),
            Self::Tag(tag) => format!("refs/tags/{}", tag),
            Self::Head => "HEAD".into(),
            Self::Commit(commit) => commit.clone(),
        }
    }
}

impl FromStr for GitRev {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        if s.to_uppercase() == "HEAD" {
            Ok(Self::Head)
        } else if let Some(tag) = s.strip_prefix("refs/tags/") {
            Ok(Self::Tag(tag.to_owned()))
        } else if let Some(branch) = s.strip_prefix("refs/heads/") {
            Ok(Self::Branch(branch.to_owned()))
        } else {
            Ok(Self::Commit(s.to_owned()))
        }
    }
}

impl Default for GitRev {
    fn default() -> Self {
        Self::Head
    }
}

/// Git source information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitSource {
    /// Url to the git repository
    pub url: GitUrl,
    /// Optionally a revision to checkout, defaults to `HEAD`
    pub rev: GitRev,
    /// Optionally a depth to clone the repository, defaults to `None`
    pub depth: Option<i32>,
    /// Optionally patches to apply to the source code
    pub patches: Vec<PathBuf>,
    /// Optionally a folder name under the `work` directory to place the source code
    pub target_directory: Option<PathBuf>,
    /// Optionally request the lfs pull in git source
    pub lfs: bool,
}
impl GitSource {
    /// Get the git url.
    pub const fn url(&self) -> &GitUrl {
        &self.url
    }

    /// Get the git revision.
    pub fn rev(&self) -> &GitRev {
        &self.rev
    }

    /// Get the git depth.
    pub const fn depth(&self) -> Option<i32> {
        self.depth
    }

    /// Get the patches.
    pub fn patches(&self) -> &[PathBuf] {
        self.patches.as_slice()
    }

    /// Get the target_directory.
    pub const fn target_directory(&self) -> Option<&PathBuf> {
        self.target_directory.as_ref()
    }

    /// Get true if source requires lfs.
    pub const fn lfs(&self) -> bool {
        self.lfs
    }
}

#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to download source from url: {0}")]
    Url(#[from] reqwest::Error),

    #[error("Url does not point to a file: {0}")]
    UrlNotFile(url::Url),

    #[error("FileSystem error: '{0}'")]
    FileSystemError(fs_extra::error::Error),

    #[error("Download could not be validated with checksum!")]
    ValidationFailed,

    #[error("File not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Could not find `patch` executable")]
    PatchNotFound,

    #[error("Failed to apply patch: {0}")]
    PatchFailed(String),

    #[error("Failed to extract archive: {0}")]
    TarExtractionError(String),

    #[error("Failed to extract zip archive: {0}")]
    ZipExtractionError(String),

    #[error("Failed to read from zip: {0}")]
    InvalidZip(String),

    #[error("Failed to run git command: {0}")]
    GitError(String),

    #[error("Failed to run git command: {0}")]
    GitErrorStr(&'static str),

    #[error("{0}")]
    UnknownError(String),

    #[error("{0}")]
    UnknownErrorStr(&'static str),

    #[error("No checksum found for url: {0}")]
    NoChecksum(url::Url),
}

/// Fetch the given repository using the host `git` executable.
pub fn fetch_repo(repo_path: &Path, url: &Url, rev: &str) -> Result<(), SourceError> {
    println!("Fetching repository from {} at {}", url, rev);
    let mut command = git_command("fetch");
    let output = command
        .args([url.to_string().as_str(), rev])
        .current_dir(repo_path)
        .output()
        .map_err(|_err| SourceError::ValidationFailed)?;

    if !output.status.success() {
        tracing::debug!("Repository fetch for revision {:?} failed!", rev);
        return Err(SourceError::GitErrorStr(
            "failed to git fetch refs from origin",
        ));
    }

    // try to suppress detached head warning
    let _ = Command::new("git")
        .current_dir(repo_path)
        .args(["config", "--local", "advice.detachedHead", "false"])
        .status();

    // checkout fetch_head
    let mut command = Command::new("git");
    let output = command
        .args(["reset", "--hard", "FETCH_HEAD"])
        .current_dir(repo_path)
        .output()
        .map_err(|_err| SourceError::ValidationFailed)?;

    if !output.status.success() {
        tracing::debug!("Repository fetch for revision {:?} failed!", rev);
        return Err(SourceError::GitErrorStr("failed to checkout FETCH_HEAD"));
    }

    tracing::debug!("Repository fetched successfully!");
    Ok(())
}

/// Create a `git` command with the given subcommand.
fn git_command(sub_cmd: &str) -> Command {
    let mut command = Command::new("git");
    command.arg(sub_cmd);

    if std::io::stdin().is_terminal() {
        command.stdout(std::process::Stdio::inherit());
        command.stderr(std::process::Stdio::inherit());
        command.arg("--progress");
    }
    command
}

/// Fetch the git repository specified by the given source and place it in the cache directory.
pub fn git_src(
    source: &GitSource,
    cache_dir: &Path,
    recipe_dir: &Path,
) -> Result<(PathBuf, String), SourceError> {
    // test if git is available locally as we fetch the git from PATH,
    if !Command::new("git")
        .arg("--version")
        .output()?
        .status
        .success()
    {
        return Err(SourceError::GitErrorStr(
            "`git` command not found in `PATH`",
        ));
    }

    // depth == -1, fetches the entire git history
    if !source.rev().is_head() && (source.depth().is_some() && source.depth() != Some(-1)) {
        return Err(SourceError::GitErrorStr(
            "use of `depth` with `rev` is invalid",
        ));
    }

    let filename = match &source.url() {
        GitUrl::Url(url) => (|| Some(url.path_segments()?.last()?.to_string()))()
            .ok_or_else(|| SourceError::GitErrorStr("failed to get filename from url"))?,
        GitUrl::Path(path) => recipe_dir
            .join(path)
            .canonicalize()?
            .file_name()
            .expect("unreachable, canonicalized paths shouldn't end with ..")
            .to_string_lossy()
            .to_string(),
    };

    let cache_name = PathBuf::from(filename);
    let cache_path = cache_dir.join(cache_name);

    let rev = source.rev().to_string();

    // Initialize or clone the repository depending on the source's git_url.
    match &source.url() {
        GitUrl::Url(url) => {
            // If the cache_path exists, initialize the repo and fetch the specified revision.
            if cache_path.exists() {
                fetch_repo(&cache_path, url, &rev)?;
            } else {
                let mut command = git_command("clone");

                command
                    .args(["--recursive", source.url().to_string().as_str()])
                    .arg(cache_path.as_os_str());

                if let Some(depth) = source.depth() {
                    command.args(["--depth", depth.to_string().as_str()]);
                }

                let output = command
                    .output()
                    .map_err(|_e| SourceError::GitErrorStr("Failed to execute clone command"))?;
                if !output.status.success() {
                    return Err(SourceError::GitErrorStr("Git clone failed for source"));
                }
            }
        }
        GitUrl::Path(path) => {
            if cache_path.exists() {
                // Remove old cache so it can be overwritten.
                if let Err(remove_error) = remove(&cache_path) {
                    tracing::error!("Failed to remove old cache directory: {}", remove_error);
                    return Err(SourceError::FileSystemError(remove_error));
                }
            }
            // git doesn't support UNC paths, hence we can't use std::fs::canonicalize
            let path = dunce::canonicalize(path).map_err(|e| {
                tracing::error!("Path not found on system: {}", e);
                SourceError::GitError(format!("{}: Path not found on system", e))
            })?;

            let path = path.to_string_lossy();
            let mut command = git_command("clone");

            command
                .arg("--recursive")
                .arg(format!("file://{}/.git", path).as_str())
                .arg(cache_path.as_os_str());

            if let Some(depth) = source.depth() {
                command.args(["--depth", depth.to_string().as_str()]);
            }

            let output = command
                .output()
                .map_err(|_| SourceError::ValidationFailed)?;

            if !output.status.success() {
                tracing::error!("Command failed: {:?}", command);
                return Err(SourceError::GitErrorStr(
                    "failed to execute clone from file",
                ));
            }
        }
    }

    // Resolve the reference and set the head to the specified revision.
    let output = Command::new("git")
        .current_dir(&cache_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|_| SourceError::GitErrorStr("git rev-parse failed"))?;

    if !output.status.success() {
        tracing::error!("Command failed: `git rev-parse \"{}\"`", &rev);
        return Err(SourceError::GitErrorStr("failed to get valid hash for rev"));
    }

    let ref_git = String::from_utf8(output.stdout)
        .map_err(|_| SourceError::GitErrorStr("failed to parse git rev as utf-8"))?
        .trim()
        .to_owned();

    tracing::info!(
        "Checked out revision: '{}' at '{}'",
        &rev,
        ref_git.as_str().trim()
    );

    Ok((cache_path, ref_git))
}
