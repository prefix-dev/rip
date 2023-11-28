use serde::{Deserialize, Serialize};
use std::ops::Deref;

mod from_env;

/// Describes the environment markers that can be used in dependency specifications to enable or
/// disable certain dependencies based on runtime environment.
///
/// Exactly the markers defined in this struct must be present during version resolution. Unknown
/// variables should raise an error.
///
/// Note that the "extra" variable is not defined in this struct because it depends on the wheel
/// that is being inspected.
///
/// The behavior and the names of the markers are described in PEP 508.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[allow(missing_docs)]
#[serde(transparent)]
pub struct Pep508EnvMakers(pub pep508_rs::MarkerEnvironment);

impl From<pep508_rs::MarkerEnvironment> for Pep508EnvMakers {
    fn from(value: pep508_rs::MarkerEnvironment) -> Self {
        Self(value)
    }
}

impl Deref for Pep508EnvMakers {
    type Target = pep508_rs::MarkerEnvironment;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
