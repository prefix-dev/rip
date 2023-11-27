//! This module contains code to parse entry points from a python package.

use crate::{extra::ParseExtraError, Extra};
use regex::Regex;
use std::{collections::HashSet, str::FromStr, sync::OnceLock};
use thiserror::Error;

/// Entry points are a mechanism for an installed python package to declare functions that can be
/// called from the command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryPoint {
    /// The name of the script that will be created
    pub script_name: String,

    /// The module in which the entry point is defined
    pub module: String,

    /// The function in the module that is the entry point
    pub function: Option<String>,
}

/// An error that might be raised when parsing [`EntryPoint`]s.
#[derive(Debug, Error)]
pub enum ParseEntryPointError {
    /// The entry point is not in the expected format.
    #[error("entry point is not in the expected format")]
    InvalidFormat,

    /// The entry points refers to an invalid extra.
    #[error(transparent)]
    ParseExtraError(#[from] ParseExtraError),
}

impl EntryPoint {
    /// Parses an entry point from a string.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::{collections::HashSet, str::FromStr};
    /// # use itertools::assert_equal;
    /// # use rattler_installs_packages::{entry_points::EntryPoint, Extra};
    /// let entry_point = EntryPoint::parse(String::from("blackd"), "blackd:patched_main", None).unwrap().unwrap();
    /// assert_eq!(entry_point.script_name, "blackd");
    /// assert_eq!(entry_point.module, "blackd");
    /// assert_eq!(entry_point.function.as_deref(), Some("patched_main"));
    ///
    /// let entry_point = EntryPoint::parse(String::from("some"), "some_module.object_ref", None).unwrap().unwrap();
    /// assert_eq!(entry_point.script_name, "some");
    /// assert_eq!(entry_point.module, "some_module.object_ref");
    /// assert_eq!(entry_point.function.as_deref(), None);
    ///
    /// let entry_point = EntryPoint::parse(String::from("blackd"), "blackd:patched_main [d]", None).unwrap().unwrap();
    /// assert_eq!(entry_point.script_name, "blackd");
    /// assert_eq!(entry_point.module, "blackd");
    /// assert_eq!(entry_point.function.as_deref(), Some("patched_main"));
    ///
    /// let entry_point = EntryPoint::parse(String::from("blackd"), "blackd:patched_main [d]", Some(&HashSet::from_iter([Extra::from_str("d").unwrap()]))).unwrap().unwrap();
    /// assert_eq!(entry_point.script_name, "blackd");
    /// assert_eq!(entry_point.module, "blackd");
    /// assert_eq!(entry_point.function.as_deref(), Some("patched_main"));
    /// ```
    pub fn parse(
        script_name: String,
        entry_point: &str,
        extras: Option<&HashSet<Extra>>,
    ) -> Result<Option<Self>, ParseEntryPointError> {
        static ENTRY_POINT_REGEX: OnceLock<Regex> = OnceLock::new();
        let entry_point_regex = ENTRY_POINT_REGEX.get_or_init(|| {
            Regex::new(r"^(?P<module>[\w\d_\-.]+)(:(?P<function>[\w\d_\-.]+))?(?:\s+\[(?P<extras>(?:[^,]+,?\s*)+)])?$").unwrap()
        });

        let captures = entry_point_regex
            .captures(entry_point)
            .ok_or(ParseEntryPointError::InvalidFormat)?;

        // Check the extras part
        if let Some(script_extras) = captures.name("extras") {
            if let Some(extras) = extras {
                let entry_point_extras = script_extras
                    .as_str()
                    .split(',')
                    .map(|extra| Extra::from_str(extra.trim()));
                for entry_point_extras in entry_point_extras {
                    if !extras.contains(&entry_point_extras?) {
                        return Ok(None);
                    }
                }
            }
        }

        Ok(Some(Self {
            script_name,
            module: captures
                .name("module")
                .expect("if the regex has captures this group must be here")
                .as_str()
                .to_string(),
            function: captures.name("function").map(|s| s.as_str().to_string()),
        }))
    }

    /// Returns a script to launch the entry-point.
    pub fn launch_script(&self) -> String {
        let (module, import_name) = match self.function.as_deref() {
            Some(func) => (self.module.as_str(), func),
            None => match self.module.split_once('.') {
                Some((module, func)) => (module, func),
                None => (self.module.as_str(), self.module.as_str()),
            },
        };

        format!(
            r##"# -*- coding: utf-8 -*-
import re
import sys
from {module} import {import_name}
if __name__ == "__main__":
    sys.argv[0] = re.sub(r"(-script\.pyw|\.exe)?$", "", sys.argv[0])
    sys.exit({import_name}())
"##,
            module = module,
            import_name = import_name
        )
    }
}
