// Implementation comes from https://github.com/njsmith/posy/blob/main/src/vocab/reqparse.rs
// Licensed under MIT or Apache-2.0

pub use self::parser::{marker, requirement, versionspec};
use super::{
    extra::Extra,
    package_name::PackageName,
    requirement::{marker, ParseExtra, Requirement},
    specifier::{CompareOp, Specifier, Specifiers},
};

peg::parser! {
    grammar parser() for str {
        rule wsp()
            = quiet!{ [' ' | '\t' ] }

        rule letter_or_digit()
            = quiet!{['A'..='Z' | 'a'..='z' | '0'..='9']} / expected!("letter or digit")

        rule _()
            = quiet!{ wsp()* }

        rule version_cmp() -> &'input str
            = $("<=" / "<" / "!=" / "==" / ">=" / ">" / "~=" / "===")

        rule version()
            = (letter_or_digit() / "-" / "_" / "." / "*" / "+" / "!")+

        rule version_one() -> Specifier
            = _ op:version_cmp() _ v:$(version())
           {?
                // peg requires &str for errors, so we can't pass through the result of
                // try_from directly
                if op == "===" {
                    Err("'===' is not implemented")
                } else {
                    Ok(Specifier {
                        // unwrap ok because: the parser rule only accepts valid operators
                        op: op.parse().unwrap(),
                       value: v.into(),
                    })
                }
            }

        rule version_many() -> Specifiers
            = specs:(version_one() ++ (_ ",")) { Specifiers(specs) }

        pub rule versionspec() -> Specifiers
            = ("(" vm:version_many() ")" { vm }) / version_many()

        rule urlspec() -> Requirement
            = "@" {? Err("direct url references not currently supported") }

        rule not_in() -> &'static str
            = "not" wsp()+ "in" { "not in" }

        rule marker_op() -> &'input str
            = _ op:(version_cmp() / $("in") / not_in()) { op }

        rule python_str_c() -> &'input str
            = $(quiet! { [' ' | '\t' | 'A'..='Z' | 'a'..='z' | '0'..='9' | '(' | ')'
                 | '.' | '{' | '}' | '-' | '_' | '*' | '#' | ':' | ';' | ','
                 | '/' | '?' | '[' | ']' | '!' | '~' | '`' | '@' | '$' | '%'
                 | '^' | '&' | '=' | '+' | '|' | '<' | '>'] })
              / expected!("printable character")

        // PEP 508 says that we don't have to support backslash escapes. It
        // also says that "existing implementations do support them", so the
        // first statement might be a lie -- maybe they're actually in use in
        // the wild. But they're complicated, so we might as well see how far
        // we can get while sticking to the spec.
        rule python_squote_str() -> &'input str
            = "'" s:$((python_str_c() / "\"")*) "'" { s }

        rule python_dquote_str() -> &'input str
            = "\"" s:$((python_str_c() / "'")*) "\"" { s }

        rule python_str() -> marker::Value
            = s:(python_squote_str() / python_dquote_str())
              { marker::Value::Literal(s.into()) }

        rule env_var(parse_extra: ParseExtra) -> marker::Value
            = var:$(
                "python_version" / "python_full_version" / "os_name"
                / "sys_platform" / "platform_release" / "platform_system"
                / "platform_version" / "platform_machine"
                / "platform_python_implementation" / "implementation_name"
                / "implementation_version" / "extra"
              )
              {?
               if ParseExtra::NotAllowed == parse_extra && var == "extra" {
                   return Err("'extra' marker is not valid in this context")
               }
               Ok(marker::Value::Variable(var.to_owned()))
              }

        // https://peps.python.org/pep-0345/#environment-markers
        rule pep345_env_var() -> marker::Value
           = var:$(
               "os.name" / "sys.platform" / "platform.version" / "platform.machine"
               / "platform.python_implementation"
             )
             {
               marker::Value::Variable(var.replace('.', "_"))
             }

        rule setuptools_env_var() -> marker::Value
           = "python_implementation"
             {
               marker::Value::Variable("platform_python_implementation".into())
             }

        rule marker_value(parse_extra: ParseExtra) -> marker::Value
            = _ v:(env_var(parse_extra) / pep345_env_var() / setuptools_env_var()
                   / python_str())
              { v }

        rule marker_expr(parse_extra: ParseExtra) -> marker::EnvMarkerExpr
            = _ "(" m:marker(parse_extra) _ ")" { m }
              / lhs:marker_value(parse_extra) op:marker_op() rhs:marker_value(parse_extra)
              {
                  use marker::EnvMarkerExpr::Operator;
                  use CompareOp::*;
                  use marker::Op::*;
                  match op {
                      "<=" => Operator { op: Compare(LessThanEqual), lhs, rhs },
                      "<" => Operator { op: Compare(StrictlyLessThan), lhs, rhs },
                      "!=" => Operator { op: Compare(NotEqual), lhs, rhs },
                      "==" => Operator { op: Compare(Equal), lhs, rhs },
                      ">=" => Operator { op: Compare(GreaterThanEqual), lhs, rhs },
                      ">" => Operator { op: Compare(StrictlyGreaterThan), lhs, rhs },
                      "~=" => Operator { op: Compare(Compatible), lhs, rhs },
                      "in" => Operator { op: In, lhs, rhs },
                      "not in" => Operator { op: NotIn, lhs, rhs },
                      _ => panic!("op can't be {:?}!", op),
                  }
              }

        rule marker_and(parse_extra: ParseExtra) -> marker::EnvMarkerExpr
            = lhs:marker_expr(parse_extra) _ "and" _ rhs:marker_and(parse_extra)
                 { marker::EnvMarkerExpr::And(Box::new(lhs), Box::new(rhs)) }
              / marker_expr(parse_extra)

        rule marker_or(parse_extra: ParseExtra) -> marker::EnvMarkerExpr
            = lhs:marker_and(parse_extra) _ "or" _ rhs:marker_or(parse_extra)
                 { marker::EnvMarkerExpr::Or(Box::new(lhs), Box::new(rhs)) }
              / marker_and(parse_extra)

        pub rule marker(parse_extra: ParseExtra) -> marker::EnvMarkerExpr
            = marker_or(parse_extra)

        rule quoted_marker(parse_extra: ParseExtra) -> marker::EnvMarkerExpr
            = ";" _ m:marker(parse_extra) { m }

        rule identifier() -> &'input str
            = $(letter_or_digit() (letter_or_digit() / "-" / "_" / ".")*)

        rule name() -> PackageName
            = n:identifier() {? n.parse().or(Err("Error parsing package name")) }

        rule extra() -> Extra
            = e:identifier() {? e.parse().or(Err("Error parsing extra name")) }

        rule extras() -> Vec<Extra>
            = "[" _ es:(extra() ** (_ "," _)) _ "]" { es }

        rule name_req(parse_extra: ParseExtra) -> Requirement
            = name:name()
              _ extras:(extras() / "" { Vec::new() })
              _ specifiers:(versionspec() / "" { Specifiers(Vec::new()) })
              _ env_marker_expr:(quoted_marker(parse_extra)?)
              {
                  Requirement {
                      name,
                      extras,
                      specifiers,
                      env_marker_expr,
                  }
              }

        rule url_req(parse_extra: ParseExtra) -> Requirement
            = name:name()
              _ extras:(extras() / "" { Vec::new() })
              _ url:urlspec()
              _ env_marker:((wsp() q:quoted_marker(parse_extra) { q })?)
            {
                // because urlspec() errors out unconditionally, up above
                unreachable!()
            }

        pub rule requirement(parse_extra: ParseExtra) -> Requirement
            = _ r:( url_req(parse_extra) / name_req(parse_extra) ) _ { r }
    }
}
