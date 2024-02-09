//! Module for parsing different HTML pages from PyPI repository
use std::str::FromStr;
use std::{borrow::Borrow, default::Default};

use crate::{types::ArtifactHashes, types::ArtifactName, types::NormalizedPackageName};
use miette::{miette, IntoDiagnostic};
use pep440_rs::VersionSpecifiers;

use rattler_digest::{parse_digest_from_hex, Sha256};

use tl::HTMLTag;
use url::Url;

use crate::types::{ArtifactInfo, DistInfoMetadata, ProjectInfo, Yanked};

/// Parse a hash from url fragment
pub fn parse_hash(s: &str) -> Option<ArtifactHashes> {
    if let Some(("sha256", hex)) = s.split_once('=') {
        Some(ArtifactHashes {
            sha256: parse_digest_from_hex::<Sha256>(hex),
        })
    } else {
        None
    }
}

fn into_artifact_info(
    base: &Url,
    normalized_package_name: &NormalizedPackageName,
    tag: &HTMLTag,
) -> Option<ArtifactInfo> {
    let attributes = tag.attributes();
    // Get first href attribute to use as filename
    let href = attributes.get("href").flatten()?.as_utf8_str();

    // Join with base
    let url = base.join(href.as_ref()).ok()?;
    let filename = url.path_segments().and_then(|mut s| s.next_back());
    let filename = filename
        .map(|s| ArtifactName::from_filename(s, normalized_package_name))?
        .ok()?;

    // We found a valid link
    let hash = url.fragment().and_then(parse_hash);
    let requires_python = attributes
        .get("data-requires-python")
        .flatten()
        // filter empty strings
        .filter(|a| !a.as_utf8_str().is_empty())
        .map(|a| {
            VersionSpecifiers::from_str(
                html_escape::decode_html_entities(a.as_utf8_str().as_ref()).as_ref(),
            )
        })
        .transpose()
        .ok()?;

    let metadata_attr = attributes
        .get("data-dist-info-metadata")
        .flatten()
        .map(|a| a.as_utf8_str());

    let dist_info_metadata = match metadata_attr {
        None => DistInfoMetadata {
            available: false,
            hashes: ArtifactHashes::default(),
        },
        Some(cow) if cow.as_ref() == "true" => DistInfoMetadata {
            available: true,
            hashes: ArtifactHashes::default(),
        },
        Some(value) => DistInfoMetadata {
            available: true,
            hashes: parse_hash(value.borrow()).unwrap_or_default(),
        },
    };

    let yanked_reason = attributes
        .get("data-yanked")
        .flatten()
        .map(|a| a.as_utf8_str());
    let yanked = match yanked_reason {
        None => Yanked {
            yanked: false,
            reason: None,
        },
        Some(reason) => Yanked {
            yanked: true,
            reason: Some(reason.to_string()),
        },
    };

    Some(ArtifactInfo {
        filename,
        url,
        is_direct_url: false,
        hashes: hash,
        requires_python,
        dist_info_metadata,
        yanked,
    })
}

/// Parses information regarding the different artifacts for a project
pub fn parse_project_info_html(base: &Url, body: &str) -> miette::Result<ProjectInfo> {
    let dom = tl::parse(body, tl::ParserOptions::default()).into_diagnostic()?;
    let variants = dom.query_selector("a");
    let mut project_info = ProjectInfo::default();

    // Find the package name from the URL
    let last_non_empty_segment = base.path_segments().and_then(|segments| {
        segments
            .rev()
            .find(|segment| !segment.is_empty())
            .map(|s| s.to_string())
    });

    // Turn into a normalized package name
    let normalized_package_name = if let Some(last_segment) = last_non_empty_segment {
        last_segment
            .parse::<NormalizedPackageName>()
            .into_diagnostic()
            .map_err(|e| {
                miette!(
                    "error parsing segment '{last_segment}' from url '{base}' into a normalized package name, error: {e}"
                )
            })?
    } else {
        return Err(miette!("no package segments found in url: '{base}'"));
    };

    // Select repository version
    project_info.meta.version = dom
        .query_selector("meta[name=\"pypi:repository-version\"]")
        // Take the first value
        .and_then(|mut v| v.next())
        // Get node access
        .and_then(|v| v.get(dom.parser()))
        // Require it to be a tag
        .and_then(|v| v.as_tag())
        // Get attributes, content specifically
        .and_then(|v| v.attributes().get("content"))
        // Get the version
        .and_then(|v| v.map(|v| v.as_utf8_str().to_string()))
        .unwrap_or_default();

    // Select base url
    let base = dom
        .query_selector("base")
        // Take the first value
        .and_then(|mut v| v.next())
        // Get node access
        .and_then(|v| v.get(dom.parser()))
        // Require it to be a tag
        .and_then(|v| v.as_tag())
        // Get attributes, href specifically
        .and_then(|v| v.attributes().get("href"))
        // Get the version
        .and_then(|v| v.map(|v| v.as_utf8_str().to_string()))
        // Parse the url
        .and_then(|v| Url::parse(&v).ok())
        // If we didn't find a base, use the one we were given
        .unwrap_or_else(|| base.clone());

    if let Some(variants) = variants {
        // Filter for <a></a> tags
        let a_tags = variants
            .filter_map(|a| a.get(dom.parser()))
            .filter_map(|h| h.as_tag());

        // Parse and add <a></a> tags
        for a in a_tags {
            let artifact_info = into_artifact_info(&base, &normalized_package_name, a);
            if let Some(artifact_info) = artifact_info {
                project_info.files.push(artifact_info);
            }
        }
    };

    Ok(project_info)
}

/// Parse package names from a pypyi repository index.
#[tracing::instrument(level = "debug", skip(body))]
pub fn parse_package_names_html(body: &str) -> miette::Result<Vec<String>> {
    let dom = tl::parse(body, tl::ParserOptions::default()).into_diagnostic()?;
    let names = dom.query_selector("a");

    if let Some(names) = names {
        let names = names
            .filter_map(|a| a.get(dom.parser()))
            .map(|node| node.inner_text(dom.parser()).to_string())
            .collect();
        Ok(names)
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_sink_simple() {
        let parsed = parse_project_info_html(
            &Url::parse("https://example.com/old-base/link").unwrap(),
            r#"<html>
                <head>
                  <meta name="pypi:repository-version" content="1.0">
                  <base href="https://example.com/new-base/">
                </head>
                <body>
                  <a href="link-1.0.tar.gz#sha256=0000000000000000000000000000000000000000000000000000000000000000">link1</a>
                  <a href="/elsewhere/link-2.0.zip" data-yanked="some reason">link2</a>
                  <a href="link-3.0.tar.gz" data-requires-python=">= 3.17">link3</a>
                  <a href="link-4.0.tar.gz" data-requires-python="">link</a>
                </body>
              </html>
            "#,
        ).unwrap();

        insta::assert_ron_snapshot!(parsed, @r###"
        ProjectInfo(
          meta: Meta(
            r#api-version: "1.0",
          ),
          files: [
            ArtifactInfo(
              filename: SDist(SDistFilename(
                distribution: "link",
                version: "1.0",
                format: TarGz,
              )),
              url: "https://example.com/new-base/link-1.0.tar.gz#sha256=0000000000000000000000000000000000000000000000000000000000000000",
              hashes: Some(ArtifactHashes(
                sha256: Some("0000000000000000000000000000000000000000000000000000000000000000"),
              )),
              r#requires-python: None,
              r#dist-info-metadata: DistInfoMetadata(
                available: false,
                hashes: ArtifactHashes(),
              ),
              yanked: Yanked(
                yanked: false,
                reason: None,
              ),
            ),
            ArtifactInfo(
              filename: SDist(SDistFilename(
                distribution: "link",
                version: "2.0",
                format: Zip,
              )),
              url: "https://example.com/elsewhere/link-2.0.zip",
              hashes: None,
              r#requires-python: None,
              r#dist-info-metadata: DistInfoMetadata(
                available: false,
                hashes: ArtifactHashes(),
              ),
              yanked: Yanked(
                yanked: true,
                reason: Some("some reason"),
              ),
            ),
            ArtifactInfo(
              filename: SDist(SDistFilename(
                distribution: "link",
                version: "3.0",
                format: TarGz,
              )),
              url: "https://example.com/new-base/link-3.0.tar.gz",
              hashes: None,
              r#requires-python: Some(">=3.17"),
              r#dist-info-metadata: DistInfoMetadata(
                available: false,
                hashes: ArtifactHashes(),
              ),
              yanked: Yanked(
                yanked: false,
                reason: None,
              ),
            ),
            ArtifactInfo(
              filename: SDist(SDistFilename(
                distribution: "link",
                version: "4.0",
                format: TarGz,
              )),
              url: "https://example.com/new-base/link-4.0.tar.gz",
              hashes: None,
              r#requires-python: None,
              r#dist-info-metadata: DistInfoMetadata(
                available: false,
                hashes: ArtifactHashes(),
              ),
              yanked: Yanked(
                yanked: false,
                reason: None,
              ),
            ),
          ],
        )
        "###);
    }

    #[test]
    fn test_package_name_parsing() {
        let html = r#"
        <html>
  <head>
    <meta name="pypi:repository-version" content="1.1">
    <title>Simple index</title>
  </head>
  <body>
    <a href="/simple/0/">0</a>
    <a href="/simple/0-0/">0-._.-._.-._.-._.-._.-._.-0</a>
    <a href="/simple/000/">000</a>
    <a href="/simple/0-0-1/">0.0.1</a>
    <a href="/simple/00101s/">00101s</a>
    <a href="/simple/00print-lol/">00print_lol</a>
    <a href="/simple/00smalinux/">00SMALINUX</a>
    <a href="/simple/0101/">0101</a>
    <a href="/simple/01changer/">01changer</a>
    <a href="/simple/01d61084-d29e-11e9-96d1-7c5cf84ffe8e/">01d61084-d29e-11e9-96d1-7c5cf84ffe8e</a>
    <a href="/simple/01-distributions/">01-distributions</a>
    <a href="/simple/021/">021</a>
    <a href="/simple/024travis-test024/">024travis-test024</a>
    <a href="/simple/02exercicio/">02exercicio</a>
    <a href="/simple/0411-test/">0411-test</a>
    <a href="/simple/0-618/">0.618</a>
    <a href="/simple/0706xiaoye/">0706xiaoye</a>
    <a href="/simple/0805nexter/">0805nexter</a>
    <a href="/simple/090807040506030201testpip/">090807040506030201testpip</a>
    <a href="/simple/0-core-client/">0-core-client</a>
    <a href="/simple/0fela/">0FELA</a>
    <a href="/simple/0html/">0html</a>
    <a href="/simple/0imap/">0imap</a>
    <a href="/simple/0lever-so/">0lever-so</a>
    <a href="/simple/0lever-utils/">0lever-utils</a>
    <a href="/simple/0-orchestrator/">0-orchestrator</a>
    <a href="/simple/0proto/">0proto</a>
    <a href="/simple/0rest/">0rest</a>
    <a href="/simple/0rss/">0rss</a>
    <a href="/simple/0wdg9nbmpm/">0wdg9nbmpm</a>
    <a href="/simple/0wneg/">0wneg</a>
    <a href="/simple/0x01-autocert-dns-aliyun/">0x01-autocert-dns-aliyun</a>
    <a href="/simple/0x01-cubic-sdk/">0x01-cubic-sdk</a>
    <a href="/simple/0x01-letsencrypt/">0x01-letsencrypt</a>
    <a href="/simple/0x0-python/">0x0-python</a>
    <a href="/simple/0x10c-asm/">0x10c-asm</a>
    <a href="/simple/0x20bf/">0x20bf</a>
    <a href="/simple/0x2nac0nda/">0x2nac0nda</a>
    <a href="/simple/0x-contract-addresses/">0x-contract-addresses</a>
    <a href="/simple/0x-contract-artifacts/">0x-contract-artifacts</a>
    <a href="/simple/0x-contract-wrappers/">0x-contract-wrappers</a>
   </body>
   </html>
        "#;

        let names = parse_package_names_html(html).unwrap();
        insta::assert_ron_snapshot!(names, @r###"
        [
          "0",
          "0-._.-._.-._.-._.-._.-._.-0",
          "000",
          "0.0.1",
          "00101s",
          "00print_lol",
          "00SMALINUX",
          "0101",
          "01changer",
          "01d61084-d29e-11e9-96d1-7c5cf84ffe8e",
          "01-distributions",
          "021",
          "024travis-test024",
          "02exercicio",
          "0411-test",
          "0.618",
          "0706xiaoye",
          "0805nexter",
          "090807040506030201testpip",
          "0-core-client",
          "0FELA",
          "0html",
          "0imap",
          "0lever-so",
          "0lever-utils",
          "0-orchestrator",
          "0proto",
          "0rest",
          "0rss",
          "0wdg9nbmpm",
          "0wneg",
          "0x01-autocert-dns-aliyun",
          "0x01-cubic-sdk",
          "0x01-letsencrypt",
          "0x0-python",
          "0x10c-asm",
          "0x20bf",
          "0x2nac0nda",
          "0x-contract-addresses",
          "0x-contract-artifacts",
          "0x-contract-wrappers",
        ]
        "###);
    }
}
