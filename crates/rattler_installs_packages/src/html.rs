// Derived from
//   https://github.com/servo/html5ever/blob/master/html5ever/examples/noop-tree-builder.rs
// Which has the following copyright header:
//
// Copyright 2014-2017 The html5ever Project Developers. See the
// COPYRIGHT file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{borrow::Borrow, borrow::Cow, collections::HashMap, default::Default, io::Read};

use crate::{ArtifactHashes, ArtifactName};
use html5ever::tendril::*;
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{expanded_name, local_name, namespace_url, ns, parse_document};
use html5ever::{Attribute, ExpandedName, LocalNameStaticSet, QualName};
use miette::IntoDiagnostic;
use once_cell::sync::Lazy;
use rattler_digest::{parse_digest_from_hex, Sha256};
use string_cache::Atom;
use url::Url;

use super::project_info::{ArtifactInfo, DistInfoMetadata, ProjectInfo, Yanked};

const META_TAG: ExpandedName = expanded_name!(html "meta");
const BASE_TAG: ExpandedName = expanded_name!(html "base");
const A_TAG: ExpandedName = expanded_name!(html "a");
const HREF_ATTR: Atom<LocalNameStaticSet> = html5ever::local_name!("href");
const NAME_ATTR: Atom<LocalNameStaticSet> = html5ever::local_name!("name");
const CONTENT_ATTR: Atom<LocalNameStaticSet> = html5ever::local_name!("content");
static REQUIRES_PYTHON_ATTR: Lazy<Atom<LocalNameStaticSet>> =
    Lazy::new(|| Atom::from("data-requires-python"));
static YANKED_ATTR: Lazy<Atom<LocalNameStaticSet>> = Lazy::new(|| Atom::from("data-yanked"));
static DATA_DIST_INFO_METADATA: Lazy<Atom<LocalNameStaticSet>> =
    Lazy::new(|| Atom::from("data-dist-info-metadata"));

struct Sink {
    next_id: usize,
    names: HashMap<usize, QualName>,
    base: Url,
    changed_base: bool,
    project_info: ProjectInfo,
}

impl Sink {
    fn get_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 2;
        id
    }
}

fn get_attr<'a>(name: &Atom<LocalNameStaticSet>, attrs: &'a Vec<Attribute>) -> Option<&'a str> {
    for attr in attrs {
        if attr.name.local == *name {
            return Some(attr.value.as_ref());
        }
    }
    None
}

fn parse_hash(s: &str) -> Option<ArtifactHashes> {
    if let Some(("sha256", hex)) = s.split_once('=') {
        Some(ArtifactHashes {
            sha256: parse_digest_from_hex::<Sha256>(hex),
        })
    } else {
        None
    }
}

impl Sink {
    fn try_parse_link(&self, url_str: &str, attrs: &Vec<Attribute>) -> Option<ArtifactInfo> {
        let url = self.base.join(url_str).ok()?;
        let filename: ArtifactName = url.path_segments()?.next_back()?.parse().ok()?;
        // We found a valid link
        let hash = url.fragment().and_then(parse_hash);
        let requires_python = get_attr(REQUIRES_PYTHON_ATTR.borrow(), attrs).map(String::from);
        let metadata_attr = get_attr(DATA_DIST_INFO_METADATA.borrow(), attrs);
        let dist_info_metadata = match metadata_attr {
            None => DistInfoMetadata {
                available: false,
                hashes: ArtifactHashes::default(),
            },
            Some("true") => DistInfoMetadata {
                available: true,
                hashes: ArtifactHashes::default(),
            },
            Some(value) => DistInfoMetadata {
                available: true,
                hashes: parse_hash(value).unwrap_or_default(),
            },
        };
        let yanked_reason = get_attr(YANKED_ATTR.borrow(), attrs);
        let yanked = match yanked_reason {
            None => Yanked {
                yanked: false,
                reason: None,
            },
            Some(reason) => Yanked {
                yanked: true,
                reason: Some(reason.into()),
            },
        };
        Some(ArtifactInfo {
            filename,
            url,
            hashes: hash,
            requires_python,
            dist_info_metadata,
            yanked,
        })
    }
}

impl TreeSink for Sink {
    type Handle = usize;
    type Output = Self;

    // This is where the actual work happens

    fn create_element(&mut self, name: QualName, attrs: Vec<Attribute>, _: ElementFlags) -> usize {
        if name.expanded() == META_TAG {
            if let Some("pypi:repository-version") = get_attr(&NAME_ATTR, &attrs) {
                if let Some(version) = get_attr(&CONTENT_ATTR, &attrs) {
                    self.project_info.meta.version = version.into();
                }
            }
        }

        if name.expanded() == BASE_TAG {
            // HTML spec says that only the first <base> is respected
            if !self.changed_base {
                self.changed_base = true;
                if let Some(new_base_str) = get_attr(&HREF_ATTR, &attrs) {
                    if let Ok(new_base) = self.base.join(new_base_str) {
                        self.base = new_base;
                    }
                }
            }
        }

        if name.expanded() == A_TAG {
            if let Some(url_str) = get_attr(&HREF_ATTR, &attrs) {
                if let Some(artifact_info) = self.try_parse_link(url_str, &attrs) {
                    self.project_info.files.push(artifact_info);
                }
            }
        }

        let id = self.get_id();
        self.names.insert(id, name);
        id
    }

    // Everything else is just boilerplate to make html5ever happy

    fn finish(self) -> Self {
        self
    }

    fn get_document(&mut self) -> usize {
        0
    }

    fn get_template_contents(&mut self, target: &usize) -> usize {
        target + 1
    }

    fn same_node(&self, x: &usize, y: &usize) -> bool {
        x == y
    }

    fn elem_name(&self, target: &usize) -> ExpandedName {
        self.names.get(target).expect("not an element").expanded()
    }

    fn create_comment(&mut self, _text: StrTendril) -> usize {
        self.get_id()
    }

    fn create_pi(&mut self, _target: StrTendril, _value: StrTendril) -> usize {
        // HTML doesn't have processing instructions
        unreachable!()
    }

    fn append_before_sibling(&mut self, _sibling: &usize, _new_node: NodeOrText<usize>) {}

    fn append_based_on_parent_node(
        &mut self,
        _element: &usize,
        _prev_element: &usize,
        _new_node: NodeOrText<usize>,
    ) {
    }

    fn parse_error(&mut self, _msg: Cow<'static, str>) {}
    fn set_quirks_mode(&mut self, _mode: QuirksMode) {}
    fn append(&mut self, _parent: &usize, _child: NodeOrText<usize>) {}

    fn append_doctype_to_document(&mut self, _: StrTendril, _: StrTendril, _: StrTendril) {}
    // This is only called on <html> and <body> tags, so we don't need to worry about it
    fn add_attrs_if_missing(&mut self, _target: &usize, _attrs: Vec<Attribute>) {}
    fn remove_from_parent(&mut self, _target: &usize) {}
    fn reparent_children(&mut self, _node: &usize, _new_parent: &usize) {}
    fn mark_script_already_started(&mut self, _node: &usize) {}
}

pub fn parse_html<T>(url: &Url, mut body: T) -> miette::Result<ProjectInfo>
where
    T: Read,
{
    let sink = Sink {
        next_id: 1,
        base: url.clone(),
        changed_base: false,
        names: HashMap::new(),
        project_info: Default::default(),
    };

    Ok(parse_document(sink, Default::default())
        // For now, we just assume that all HTML is utf-8... this might bite us
        // eventually, but hopefully it's true for the package index situation of
        // API-responses-masquerading-as-HTML
        .from_utf8()
        .read_from(&mut body)
        .into_diagnostic()?
        .project_info)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_sink_simple() {
        let parsed = parse_html(
            &Url::parse("https://example.com/old-base/").unwrap(),
            "text/html",
            br#"<html>
                <head>
                  <meta name="pypi:repository-version" content="1.0">
                  <base href="https://example.com/new-base/">
                </head>
                <body>
                  <a href="link1-1.0.tar.gz#sha256=0000000000000000000000000000000000000000000000000000000000000000">link1</a>
                  <a href="/elsewhere/link2-2.0.zip" data-yanked="some reason">link2</a>
                  <a href="link3-3.0.tar.gz" data-requires-python=">= 3.17">link3</a>
                </body>
              </html>
            "# as &[u8],
        ).unwrap();

        insta::assert_ron_snapshot!(parsed, @r###"
        ProjectInfo(
          meta: Meta(
            version: "1.0",
          ),
          artifacts: [
            ArtifactInfo(
              name: "link1-1.0.tar.gz",
              url: "https://example.com/new-base/link1-1.0.tar.gz#sha256=0000000000000000000000000000000000000000000000000000000000000000",
              hash: Some("sha256=0000000000000000000000000000000000000000000000000000000000000000"),
              requires_python: None,
              dist_info_metadata: DistInfoMetadata(
                available: false,
                hash: None,
              ),
              yanked: Yanked(
                yanked: false,
                reason: None,
              ),
            ),
            ArtifactInfo(
              name: "link2-2.0.zip",
              url: "https://example.com/elsewhere/link2-2.0.zip",
              hash: None,
              requires_python: None,
              dist_info_metadata: DistInfoMetadata(
                available: false,
                hash: None,
              ),
              yanked: Yanked(
                yanked: true,
                reason: Some("some reason"),
              ),
            ),
            ArtifactInfo(
              name: "link3-3.0.tar.gz",
              url: "https://example.com/new-base/link3-3.0.tar.gz",
              hash: None,
              requires_python: Some(">= 3.17"),
              dist_info_metadata: DistInfoMetadata(
                available: false,
                hash: None,
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
}
