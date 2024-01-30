use std::collections::HashMap;
use std::fmt;
use std::{
    fmt::{Display, Formatter},
    io::IsTerminal,
    path::PathBuf,
    process::Command,
    str::FromStr,
};

use fs_extra::dir::remove;
use miette::IntoDiagnostic;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
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

    pub fn get_commit(&self) -> String {
        match self {
            Self::Branch(branch) => branch.clone(),
            Self::Tag(tag) => tag.clone(),
            Self::Head => "HEAD".into(),
            Self::Commit(commit) => commit.clone(),
        }
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
/// A struct which store
/// cleaned url with revision and subdirectory
/// parsed from
/// git+https://github.com/example/repo.git@1.0.0#subdirectry=some
pub struct ParsedUrl {
    /// Url to the git repository
    pub git_url: GitUrl,
    /// Url to the git repository
    pub url: String,
    /// Revision to checkout
    pub revision: Option<String>,
    /// subdirectory to build package
    pub subdirectory: Option<String>,
}

impl ParsedUrl {
    pub fn new(url: &Url) -> miette::Result<Self> {
        let url_str = url.as_str();

        let revision = Self::extract_revision_from_git_url(url_str);
        let subdirectory = Self::subdirectory_fragment(url_str);
        let mut clean_url = Self::clean_url(url_str);

        let git_url = if clean_url.contains("git+https") {
            clean_url = clean_url.replace("git+https", "https");
            let url = Url::from_str(&clean_url).into_diagnostic()?;
            GitUrl::Url(url)
        } else {
            let path = url.path();
            clean_url = path.replace(".git", "");
            let path = PathBuf::from_str(&clean_url).into_diagnostic()?;
            GitUrl::Path(path)
        };

        Ok(ParsedUrl {
            git_url,
            url: clean_url,
            revision,
            subdirectory,
        })
    }

    /// Extract git revision if it's present
    /// and return url without revision and the revision
    fn extract_revision_from_git_url(url: &str) -> Option<String> {
        // Split the string at '@' and take the second part
        let rev = if url.contains('@') {
            let splitted: Vec<&str> = url.split('@').collect();
            if let Some((rev, _)) = splitted.split_last() {
                Some(String::from(*rev))
            } else {
                None
            }
        } else {
            None
        };

        rev
    }

    fn subdirectory_fragment(url: &str) -> Option<String> {
        let subdirectory_fragment_re = Regex::new(r#"[#&]subdirectory=([^&]*)"#).unwrap();

        if let Some(captures) = subdirectory_fragment_re.captures(url) {
            if let Some(subdirectory) = captures.get(1) {
                return Some(subdirectory.as_str().to_string());
            }
        }
        None
    }

    fn clean_url(url: &str) -> String {
        // Find the index of ".git" in the repository URL, or use the length if ".git" is not present
        let repo_index = url
            .find(".git")
            .map(|index| index + 4)
            .unwrap_or_else(|| url.len());

        // Remove everything after ".git"
        let clean_url = url.chars().take(repo_index).collect();

        clean_url
    }
}

/// Git source information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitSource {
    /// Url to the git repository
    pub url: GitUrl,
    /// Optionally a revision to checkout, defaults to `HEAD`
    pub rev: Option<String>,
}
impl GitSource {
    /// Get the git url.
    pub const fn url(&self) -> &GitUrl {
        &self.url
    }
}

#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to download source from url: {0}")]
    Url(#[from] reqwest::Error),

    #[error("FileSystem error: '{0}'")]
    FileSystemError(fs_extra::error::Error),

    #[error("Download could not be validated with checksum!")]
    ValidationFailed,

    #[error("Failed to run git command: {0}")]
    GitError(String),

    #[error("Failed to run git command: {0}")]
    GitErrorStr(&'static str),
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

fn git_version() -> miette::Result<(u8, u8)> {
    let output = Command::new("git")
        .arg("version")
        .output()
        .into_diagnostic()?;

    let output_str = String::from_utf8_lossy(&output.stdout);
    let regex = Regex::new(r"^git version (\d+)\.(\d+)(?:\s+\(.*\))?*").into_diagnostic()?;
    let captures = regex.captures(&output_str);
    if let Some(version) = captures {
        let major = u8::from_str(&version[1]).into_diagnostic()?;
        let minor = u8::from_str(&version[2]).into_diagnostic()?;
        Ok((major, minor))
    } else {
        Err(miette::miette!(
            help = "Can't parse git version.",
            "{}",
            output_str
        ))
    }
}

fn support_partial_clone() -> miette::Result<bool> {
    let version = git_version()?;
    if version >= (2, 17) {
        return Ok(true);
    }
    Ok(false)
}

fn get_revision_sha(dest: &PathBuf, rev: Option<String>) -> Result<GitRev, SourceError> {
    // Pass rev to pre-filter the list.
    let rev = if let Some(rev) = rev {
        rev
    } else {
        return Ok(GitRev::Head);
    };

    let output = Command::new("git")
        .args(["show-ref", &rev])
        .current_dir(dest)
        .output()?;

    let output_str = String::from_utf8_lossy(&output.stdout);
    let refs: HashMap<_, _> = output_str
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.splitn(2, ' ');
            let ref_sha = parts.next().unwrap().to_string();
            let ref_name = parts.next().unwrap().to_string();
            (ref_name, ref_sha)
        })
        .collect();

    let branch_ref = format!("refs/remotes/origin/{}", rev);
    let tag_ref = format!("refs/tags/{}", rev);

    let sha = refs.get(&branch_ref).cloned();
    if let Some(sha) = sha {
        return Ok(GitRev::Branch(sha));
    }

    let sha = refs.get(&tag_ref).cloned();
    if let Some(sha) = sha {
        return Ok(GitRev::Tag(sha));
    }

    Ok(GitRev::Commit(rev.to_owned()))
}

/// Fetch the git repository specified by the given source and place it in the cache directory.
pub fn git_clone(source: &GitSource, tmp_dir: &TempDir) -> Result<PathBuf, SourceError> {
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

    let cache_dir = tmp_dir.path().join("rip-git-cache");
    let recipe_dir = tmp_dir.path().join("rip-clone-dir");

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

    // Initialize or clone the repository depending on the source's git_url.
    match &source.url() {
        GitUrl::Url(_) => {
            // If the cache_path exists, initialize the repo and fetch the specified revision.
            if !cache_path.exists() {
                let mut command = git_command("clone");
                if support_partial_clone().is_ok() {
                    command.arg("--filter=blob:none");
                } else {
                    command.arg("--recursive");
                }

                command
                    .arg(source.url().to_string().as_str())
                    .arg(cache_path.as_os_str());

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

            let mut command = git_command("clone");

            command
                .arg("--recursive")
                .arg(path)
                .arg(cache_path.as_os_str());

            let output = command
                .output()
                .map_err(|_| SourceError::ValidationFailed)?;

            if !output.status.success() {
                tracing::error!("Command failed: {:?}", command);
                let err = String::from_utf8(output.stdout).unwrap();
                let err_msg = format!(
                    "failed to execute clone from file {:?} {:?}",
                    output.status, err
                );
                return Err(SourceError::GitError(err_msg));
            }
        }
    };

    let git_rev = get_revision_sha(&cache_path, source.rev.clone())?;

    let mut checkout = git_command("checkout");

    let cmd = if !git_rev.is_head() {
        Some(checkout.args(["-q", git_rev.get_commit().as_str()]))
    } else {
        None
    };

    if let Some(cmd) = cmd {
        let output = cmd
            .current_dir(&cache_path)
            .output()
            .map_err(|_| SourceError::GitErrorStr("git checkout failed"))?;

        if !output.status.success() {
            tracing::error!(
                "Command failed: `git checkout \"{}\"`",
                &git_rev.to_string()
            );
            return Err(SourceError::GitErrorStr(
                "failed to checkout for a valid rev",
            ));
        }
    }

    // update submodules
    if cache_path.join(".gitmodules").exists() {
        let mut submodule = git_command("submodule");
        let output = submodule
            .current_dir(&cache_path)
            .arg("update")
            .args(["--init", "--recursive", "-q"])
            .output()
            .map_err(|_| SourceError::GitErrorStr("git submodule update failed"))?;

        if !output.status.success() {
            return Err(SourceError::GitErrorStr("failed to update git module"));
        }
    }

    Ok(cache_path)
}
