use serde::{Deserialize, Serialize};
use url::Url;

/// Specifies the PyPa `direct_url.json` format.
/// See: <https://packaging.python.org/en/latest/specifications/direct-url-data-structure/>
///
#[derive(Debug, Serialize, Deserialize)]
#[serde_with::skip_serializing_none]
pub struct DirectUrlJson {
    /// Url to the source.
    pub url: Url,
    /// Information about the source.
    #[serde(flatten)]
    pub source: DirectUrlSource,
}

/// Specifies the source of a direct url.
///
/// currently we do not support the deprecated `hash` field
#[derive(Debug, Serialize, Deserialize)]
pub enum DirectUrlSource {
    #[serde(rename = "archive_info")]
    /// Information about the archive file.
    Archive {
        /// Hashes of the archive file.
        hashes: DirectUrlHashes,
    },
    /// Information about a source from a VCS directly
    #[serde(rename = "vcs_info")]
    Vcs {
        /// The VCS used
        vcs: DirectUrlVcs,
        /// Revision of the source
        requested_revision: Option<String>,
        /// Actual commit
        commit_id: String,
    },
    /// Information about a local directory source
    #[serde(rename = "dir_info")]
    Dir {
        /// Is this a editable source
        /// See: <https://pip.pypa.io/en/stable/topics/local-project-installs/>
        editable: Option<bool>,
    },
}

/// Hashes for internal archive files.
/// multiple hashes can be included but per recommendation only sha256 should be used.
#[derive(Debug, Serialize, Deserialize)]
pub struct DirectUrlHashes {
    /// Sha256 hash of the archive file.
    pub sha256: String,
}

/// Name of the VCS in a DirectUrlSource
#[derive(Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum DirectUrlVcs {
    #[serde(rename = "git")]
    Git,
    #[serde(rename = "svn")]
    Svn,
    #[serde(rename = "bzr")]
    Bazaar,
    #[serde(rename = "hg")]
    Mercurial,
}

#[cfg(test)]
mod tests {
    use crate::types::direct_url_json::DirectUrlJson;

    /// Tests if json outputs aligns with the examples at:
    /// https://packaging.python.org/en/latest/specifications/direct-url-data-structure/
    /// try to parse the example cases from there
    #[test]
    pub fn test_examples_pypa() {
        // Source archive:
        let example = r#"
        {
            "url": "https://github.com/pypa/pip/archive/1.3.1.zip",
            "archive_info": {
                "hashes": {
                    "sha256": "2dc6b5a470a1bde68946f263f1af1515a2574a150a30d6ce02c6ff742fcc0db8"
                }
            }
        }
        "#;
        serde_json::from_str::<DirectUrlJson>(example).unwrap();

        // Git URL with tag and commit-hash:
        let example = r#"
        {
            "url": "https://github.com/pypa/pip.git",
            "vcs_info": {
                "vcs": "git",
                "requested_revision": "1.3.1",
                "commit_id": "7921be1537eac1e97bc40179a57f0349c2aee67d"
            }
        }
        "#;
        serde_json::from_str::<DirectUrlJson>(example).unwrap();

        // Local directory:
        let example = r#"
        {
            "url": "file:///home/user/project",
            "dir_info": {}
        }
        "#;
        serde_json::from_str::<DirectUrlJson>(example).unwrap();

        // Local directory in editable mode:
        let example = r#"
        {
            "url": "file:///home/user/project",
            "dir_info": {
                "editable": true
            }
        }
        "#;
        serde_json::from_str::<DirectUrlJson>(example).unwrap();
    }
}
