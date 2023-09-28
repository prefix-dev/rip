// Implementation comes from https://github.com/njsmith/posy/blob/main/src/vocab/requirement.rs
// Licensed under MIT or Apache-2.0

// There are two kinds of special exact version constraints that aren't often
// used, and whose semantics are a bit unclear:
//
//  === "some string"
//  @ some_url
//
// Not sure if we should bother supporting them. For === they're easy to parse
// and represent (same as all the other binary comparisons), but I don't know
// what the semantics is, b/c we fully parse all versions. PEP 440 says "The
// primary use case ... is to allow for specifying a version which cannot
// otherwise by represented by this PEP". Maybe if we find ourselves supporting
// LegacyVersion-type versions, we should add this then? Though even then, I'm not sure
// we can convince pubgrub to handle it.
//
// If we do want to parse @ syntax, the problem is more: how do we represent
// them? Because it *replaces* version constraints, so I guess inside the
// Requirement object we'd need something like:
//
//   enum Specifiers {
//      Direct(Url),
//      Index(Vec<Specifier>),
//   }
//
// ? But then that complexity propagates through to everything that uses
// Requirements.
//
// Also, I don't think @ is allowed in public indexes like PyPI?
//
// NB: if we do decide to handle '@', then PEP 508 includes an entire copy of
// (some version of) the standard URL syntax. We don't want to do that, both
// because it's wildly more complicated than required, and because there are
// >3 different standards purpoting to define URL syntax and we don't want to
// take sides. But! The 'packaging' module just does
//
//    URI = Regex(r"[^ ]+")("url")
//
// ...so we can just steal some version of that.
//
// For resolving, we can treat it as a magic package that provides/depends on the
// version it declares, so it can satisfy other dependencies that use the name or
// versions.

use super::specifier::CompareOp;
use crate::extra::Extra;
use crate::package_name::PackageName;
use crate::specifier::Specifiers;
use miette::{IntoDiagnostic, WrapErr};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::borrow::Borrow;
use std::fmt::Display;
use std::ops::Deref;
use std::str::FromStr;

pub mod marker {
    use crate::extra::Extra;
    use std::collections::HashMap;
    use std::fmt::Display;
    use std::{borrow::Borrow, hash::Hash};

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub enum Value {
        Variable(String),
        Literal(String),
    }

    #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
    pub enum Op {
        Compare(CompareOp),
        In,
        NotIn,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub enum EnvMarkerExpr {
        And(Box<EnvMarkerExpr>, Box<EnvMarkerExpr>),
        Or(Box<EnvMarkerExpr>, Box<EnvMarkerExpr>),
        Operator { op: Op, lhs: Value, rhs: Value },
    }

    pub trait Env {
        fn get_marker_var(&self, var: &str) -> Option<&str>;
    }

    impl<T: Borrow<str> + Eq + Hash> Env for HashMap<T, T> {
        fn get_marker_var(&self, var: &str) -> Option<&str> {
            self.get(var).map(|s| s.borrow())
        }
    }

    impl Value {
        pub fn eval<'a>(&'a self, env: &'a dyn Env) -> miette::Result<&'a str> {
            match self {
                Value::Variable(varname) => env.get_marker_var(varname).ok_or_else(|| {
                    miette::miette!("no environment marker variable named '{}'", varname)
                }),
                Value::Literal(s) => Ok(s),
            }
        }

        pub fn is_extra(&self) -> bool {
            match self {
                Value::Variable(varname) => varname == "extra",
                Value::Literal(_) => false,
            }
        }
    }

    impl Display for Value {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Value::Variable(var) => write!(f, "{}", var),
                Value::Literal(literal) => {
                    if literal.contains('"') {
                        write!(f, "'{}'", literal)
                    } else {
                        write!(f, "\"{}\"", literal)
                    }
                }
            }
        }
    }

    impl EnvMarkerExpr {
        pub fn eval(&self, env: &dyn Env) -> miette::Result<bool> {
            Ok(match self {
                EnvMarkerExpr::And(lhs, rhs) => lhs.eval(env)? && rhs.eval(env)?,
                EnvMarkerExpr::Or(lhs, rhs) => lhs.eval(env)? || rhs.eval(env)?,
                EnvMarkerExpr::Operator { op, lhs, rhs } => {
                    let mut lhs_val = lhs.eval(env)?;
                    let mut rhs_val = rhs.eval(env)?;
                    // special hack for comparisons involving the magic 'extra'
                    // variable: always normalize both sides (see PEP 685)
                    let lhs_holder: String;
                    let rhs_holder: String;
                    if lhs.is_extra() {
                        if let Ok(extra) = Extra::from_str(rhs_val) {
                            rhs_holder = extra.as_str().to_string();
                            rhs_val = rhs_holder.as_str();
                        }
                    }
                    if rhs.is_extra() {
                        if let Ok(extra) = Extra::from_str(lhs_val) {
                            lhs_holder = extra.as_str().to_string();
                            lhs_val = lhs_holder.as_str();
                        }
                    }
                    match op {
                        Op::In => rhs_val.contains(lhs_val),
                        Op::NotIn => !rhs_val.contains(lhs_val),
                        Op::Compare(op) => {
                            // If both sides can be parsed as versions (or the RHS can
                            // be parsed as a wildcard with a wildcard-accepting op),
                            // then we do a version comparison
                            if let Ok(lhs_ver) = lhs_val.parse() {
                                if let Ok(rhs_ranges) = op.ranges(rhs_val) {
                                    return Ok(rhs_ranges
                                        .into_iter()
                                        .any(|r| r.contains(&lhs_ver)));
                                }
                            }
                            use CompareOp::*;
                            match op {
                                LessThanEqual => lhs_val <= rhs_val,
                                StrictlyLessThan => lhs_val < rhs_val,
                                NotEqual => lhs_val != rhs_val,
                                Equal => lhs_val == rhs_val,
                                GreaterThanEqual => lhs_val >= rhs_val,
                                StrictlyGreaterThan => lhs_val > rhs_val,
                                Compatible => {
                                    miette::bail!("~= requires valid version strings")
                                }
                            }
                        }
                    }
                }
            })
        }
    }

    impl Display for EnvMarkerExpr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                EnvMarkerExpr::And(lhs, rhs) => write!(f, "({} and {})", lhs, rhs)?,
                EnvMarkerExpr::Or(lhs, rhs) => write!(f, "({} or {})", lhs, rhs)?,
                EnvMarkerExpr::Operator { op, lhs, rhs } => write!(
                    f,
                    "{} {} {}",
                    lhs,
                    match op {
                        Op::Compare(compare_op) => compare_op.to_string(),
                        Op::In => "in".to_string(),
                        Op::NotIn => "not in".to_string(),
                    },
                    rhs,
                )?,
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, SerializeDisplay, DeserializeFromStr)]
pub struct StandaloneMarkerExpr(pub marker::EnvMarkerExpr);

impl Display for StandaloneMarkerExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for StandaloneMarkerExpr {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let expr = super::reqparse::marker(value, ParseExtraInEnv::NotAllowed)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed parsing env marker expression {:?}", value))?;
        Ok(StandaloneMarkerExpr(expr))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ParseExtraInEnv {
    Allowed,
    NotAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub name: PackageName,
    pub extras: Vec<Extra>,
    pub specifiers: Specifiers,
    pub env_marker_expr: Option<marker::EnvMarkerExpr>,
}

impl Requirement {
    pub fn parse(input: &str, parse_extra: ParseExtraInEnv) -> miette::Result<Requirement> {
        let req = super::reqparse::requirement(input, parse_extra)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed parsing requirement string {:?})", input))?;
        Ok(req)
    }
}

impl Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name.as_source_str())?;
        if !self.extras.is_empty() {
            write!(f, "[")?;
            let mut first = true;
            for extra in &self.extras {
                if !first {
                    write!(f, ",")?;
                }
                first = false;
                write!(f, "{}", extra.as_source_str())?;
            }
            write!(f, "]")?;
        }
        if !self.specifiers.0.is_empty() {
            write!(f, " {}", self.specifiers)?;
        }
        if let Some(env_marker) = &self.env_marker_expr {
            write!(f, "; {}", env_marker)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, DeserializeFromStr, SerializeDisplay)]
pub struct PackageRequirement(Requirement);

impl PackageRequirement {
    pub fn into_inner(self) -> Requirement {
        self.0
    }

    pub fn as_inner(&self) -> &Requirement {
        &self.0
    }
}

impl Display for PackageRequirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for PackageRequirement {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(PackageRequirement(Requirement::parse(
            value,
            ParseExtraInEnv::Allowed,
        )?))
    }
}

impl AsRef<Requirement> for PackageRequirement {
    fn as_ref(&self) -> &Requirement {
        &self.0
    }
}

impl Borrow<Requirement> for PackageRequirement {
    fn borrow(&self) -> &Requirement {
        &self.0
    }
}

impl Deref for PackageRequirement {
    type Target = Requirement;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, DeserializeFromStr, SerializeDisplay)]
pub struct UserRequirement(Requirement);

impl UserRequirement {
    pub fn into_inner(self) -> Requirement {
        self.0
    }

    pub fn as_inner(&self) -> &Requirement {
        &self.0
    }
}

impl Display for UserRequirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for UserRequirement {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(UserRequirement(Requirement::parse(
            value,
            ParseExtraInEnv::NotAllowed,
        )?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, DeserializeFromStr, SerializeDisplay)]
pub struct PythonRequirement(Requirement);

impl Display for PythonRequirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<Requirement> for PythonRequirement {
    type Error = miette::Report;

    fn try_from(r: Requirement) -> Result<Self, Self::Error> {
        if !r.extras.is_empty() {
            miette::bail!("can't have extras on python requirement {}", r);
        }
        if r.env_marker_expr.is_some() {
            miette::bail!(
                "can't have env marker restrictions on python requirement {}",
                r
            );
        }
        Ok(PythonRequirement(r))
    }
}

impl FromStr for PythonRequirement {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let r = Requirement::parse(value, ParseExtraInEnv::NotAllowed)?;
        r.try_into()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_package_requirement_basics() {
        let r: PackageRequirement =
            "twisted[tls] >= 20, != 20.1.*; python_version >= '3' and extra == 'hi'"
                .parse()
                .unwrap();
        insta::assert_ron_snapshot!(
            r,
            @r###""twisted[tls] >= 20, != 20.1.*; (python_version >= \"3\" and extra == \"hi\")""###
        );
    }

    #[test]
    fn test_user_requirement_basics() {
        assert!(UserRequirement::from_str("twisted; extra == 'hi'").is_err());
        let r: UserRequirement = "twisted[tls] >= 20, != 20.1.*; python_version >= '3'"
            .parse()
            .unwrap();
        insta::assert_ron_snapshot!(
            r,
            @r###""twisted[tls] >= 20, != 20.1.*; python_version >= \"3\"""###
        );
    }

    #[test]
    fn test_no_paren_chained_operators() {
        // The formal grammar in PEP 508 fails to parse expressions like:
        //   "_ and _ and _"
        //   "_ or _ or _"
        let r: PackageRequirement =
            "foo; os_name == 'a' and os_name == 'b' and os_name == 'c' or os_name == 'd' or os_name == 'e'"
                .parse()
                .unwrap();
        insta::assert_ron_snapshot!(
            r,
            @r###""foo; ((os_name == \"a\" and (os_name == \"b\" and os_name == \"c\")) or (os_name == \"d\" or os_name == \"e\"))""###
        );
    }

    #[test]
    fn test_legacy_env_marker_vars() {
        // should parse these, and normalize them to their PEP 508 equivalents
        let r: PackageRequirement = "foo; os.name == 'nt' and python_implementation == 'pypy'"
            .parse()
            .unwrap();
        insta::assert_ron_snapshot!(r, @r###""foo; (os_name == \"nt\" and platform_python_implementation == \"pypy\")""###);
    }

    #[test]
    fn test_requirement_roundtrip() {
        let reqs = vec![
            "foo",
            "foo (>=2, <3)",
            "foo >=1,<2, ~=3.1, ==0.0.*, !=7, >10, <= 8",
            "foo[bar,baz, quux]",
            "foo; python_version >= '3' and sys_platform == \"win32\" or sys_platform != \"linux\"",
            "foo.bar-baz (~=7); 'win' in sys_platform or 'linux' not in sys_platform",
        ];
        for req in reqs {
            let ur: UserRequirement = req.parse().unwrap();
            assert_eq!(ur, ur.to_string().parse().unwrap());

            let pr: PackageRequirement = req.parse().unwrap();
            assert_eq!(pr, pr.to_string().parse().unwrap());
        }
    }

    #[test]
    fn test_extra_normalization() {
        let r: PackageRequirement = "foo; extra == 'HeLlO' and extra in 'hElLoWorld'"
            .parse()
            .unwrap();
        let env = HashMap::from([("extra", "hello")]);
        assert!(r.env_marker_expr.as_ref().unwrap().eval(&env).unwrap());
    }
}
