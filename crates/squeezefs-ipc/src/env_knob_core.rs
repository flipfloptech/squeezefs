//! The ONE env-knob parsing convention (ENG-10, pre-RC spec §10).
//!
//! Before this module the tree parsed ~117 `SQUEEZEFS_*` knobs three
//! incompatible ways: 56 sites took a silent default on a malformed value
//! (a typo'd tuning knob was indistinguishable from not setting it), 5
//! `panic!`ed and killed the mount, 3 returned a loud error — and 19
//! boolean flags were **presence-based**, so `SQUEEZEFS_FREE_FORENSICS=0`
//! *enabled* the feature. Operators cannot be expected to remember which
//! spelling a given knob happens to implement.
//!
//! The convention, in one place:
//!
//! 1. **Unset, empty, or whitespace-only means absent.** `VAR=` is the
//!    shell idiom for "not set" and must never mean "set to garbage".
//! 2. **Explicit values win verbatim**, then percentages, then derivations
//!    (the pre-existing ipc-cap precedence, unchanged).
//! 3. **A malformed value is refused loudly, never silently defaulted.**
//!    The parse functions here are pure and return [`KnobError`]; the
//!    daemon's registry (`squeezefs::env_knobs`) validates the whole
//!    environment at startup and **refuses to start**, naming every
//!    offending knob. The LD_PRELOAD shim, which must never kill its host
//!    application over an env typo, announces loudly and keeps its
//!    documented default — a deliberate, documented asymmetry.
//! 4. **Booleans have ONE spelling set**: `1`/`true`/`yes`/`on` and
//!    `0`/`false`/`no`/`off`, ASCII-case-insensitive. `=0` therefore
//!    disables everywhere, including the knobs whose default is ON.
//!
//! This file is dependency-free and pure (no logging, no env reads, no
//! syscalls) so it can be `#[path]`-shared into the root crate, the fuse3
//! fork and the preload shim — the `numa_core`/`thp` production-sharing
//! precedent. Each consumer decides how to be loud; none of them get to
//! decide what a value MEANS.

use std::fmt;

/// A knob whose value could not be parsed. Carries everything the loud
/// refusal needs: the name, the offending value verbatim, and what was
/// expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnobError {
    /// The environment variable name.
    pub key: String,
    /// The rejected value, verbatim (quoted when printed — trailing
    /// whitespace and invisible characters are a real cause).
    pub raw: String,
    /// What the knob accepts, phrased for an operator.
    pub want: String,
}

impl fmt::Display for KnobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}='{}' is invalid: {}", self.key, self.raw, self.want)
    }
}

impl std::error::Error for KnobError {}

impl KnobError {
    fn new(key: &str, raw: &str, want: impl Into<String>) -> Self {
        Self {
            key: key.to_string(),
            raw: raw.to_string(),
            want: want.into(),
        }
    }
}

/// The absent rule (convention 1): unset, empty, or whitespace-only is
/// absent. Returns the TRIMMED value otherwise, so every knob agrees on
/// what `" 1 "` means.
pub fn present(raw: Option<&str>) -> Option<&str> {
    let v = raw?.trim();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// The accepted boolean spellings, in the order they are reported.
pub const BOOL_TRUE: [&str; 4] = ["1", "true", "yes", "on"];
/// The accepted false spellings.
pub const BOOL_FALSE: [&str; 4] = ["0", "false", "no", "off"];

/// The ONE boolean convention (convention 4). `Ok(None)` = absent.
pub fn parse_bool(key: &str, raw: Option<&str>) -> Result<Option<bool>, KnobError> {
    let Some(v) = present(raw) else {
        return Ok(None);
    };
    if BOOL_TRUE.iter().any(|t| v.eq_ignore_ascii_case(t)) {
        return Ok(Some(true));
    }
    if BOOL_FALSE.iter().any(|t| v.eq_ignore_ascii_case(t)) {
        return Ok(Some(false));
    }
    Err(KnobError::new(
        key,
        v,
        "expected a boolean — 1/true/yes/on or 0/false/no/off",
    ))
}

/// Integer parse for any `FromStr` integer type (convention 3).
/// `Ok(None)` = absent.
pub fn parse_int<T>(key: &str, raw: Option<&str>) -> Result<Option<T>, KnobError>
where
    T: std::str::FromStr,
{
    let Some(v) = present(raw) else {
        return Ok(None);
    };
    match v.parse::<T>() {
        Ok(n) => Ok(Some(n)),
        Err(_) => Err(KnobError::new(
            key,
            v,
            format!("expected an integer ({})", std::any::type_name::<T>()),
        )),
    }
}

/// Integer parse with an inclusive admissible range. Out-of-range is a
/// REFUSAL, not a clamp: a knob set to 10× the maximum is a mistake worth
/// naming, and silently clamping is how "I set it and nothing happened"
/// bug reports are born. (Derived-value clamping inside a sizing function
/// is a different thing and stays where it is.)
pub fn parse_int_in<T>(key: &str, raw: Option<&str>, lo: T, hi: T) -> Result<Option<T>, KnobError>
where
    T: std::str::FromStr + PartialOrd + fmt::Display + Copy,
{
    let Some(v) = present(raw) else {
        return Ok(None);
    };
    // The range is the whole message, for junk and out-of-range alike: an
    // operator needs to see what IS admissible, not the Rust type name.
    let bad = || KnobError::new(key, v, format!("expected an integer in {lo}..={hi}"));
    let n: T = v.parse().map_err(|_| bad())?;
    if n < lo || n > hi {
        return Err(bad());
    }
    Ok(Some(n))
}

/// One-of-a-set parse. `Ok(None)` = absent; the returned `&str` is the
/// matched (canonical) variant, so callers match on a known value.
pub fn parse_enum(
    key: &str,
    raw: Option<&str>,
    allowed: &[&'static str],
) -> Result<Option<&'static str>, KnobError> {
    let Some(v) = present(raw) else {
        return Ok(None);
    };
    for a in allowed {
        if v.eq_ignore_ascii_case(a) {
            return Ok(Some(a));
        }
    }
    Err(KnobError::new(
        key,
        v,
        format!("expected one of {}", allowed.join("|")),
    ))
}

/// A non-empty free-form value (paths, URIs, comma lists): absent or
/// present, never malformed — the consumer validates the content (a
/// nonexistent path is its own, better error).
pub fn parse_str(raw: Option<&str>) -> Option<&str> {
    present(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_whitespace_are_absent() {
        assert_eq!(present(None), None);
        assert_eq!(present(Some("")), None);
        assert_eq!(present(Some("   ")), None);
        assert_eq!(present(Some("\t\n")), None);
        assert_eq!(present(Some(" 7 ")), Some("7"), "values are trimmed");
    }

    #[test]
    fn every_bool_spelling_parses_case_insensitively() {
        for t in BOOL_TRUE {
            assert_eq!(parse_bool("K", Some(t)), Ok(Some(true)), "{t}");
            assert_eq!(
                parse_bool("K", Some(&t.to_uppercase())),
                Ok(Some(true)),
                "{t} uppercased"
            );
        }
        for f in BOOL_FALSE {
            assert_eq!(parse_bool("K", Some(f)), Ok(Some(false)), "{f}");
            assert_eq!(
                parse_bool("K", Some(&f.to_uppercase())),
                Ok(Some(false)),
                "{f} uppercased"
            );
        }
        assert_eq!(parse_bool("K", None), Ok(None));
        assert_eq!(parse_bool("K", Some("  ")), Ok(None));
    }

    /// The ENG-10 defect, pinned: `=0` must DISABLE, never enable. (The
    /// old presence-based sites read `env::var(..).is_ok()`.)
    #[test]
    fn zero_disables_it_never_enables() {
        assert_eq!(parse_bool("K", Some("0")), Ok(Some(false)));
        assert_eq!(parse_bool("K", Some("off")), Ok(Some(false)));
        assert_eq!(parse_bool("K", Some("FALSE")), Ok(Some(false)));
    }

    #[test]
    fn a_bad_bool_is_refused_with_the_value_and_the_expectation() {
        let e = parse_bool("SQUEEZEFS_X", Some("yess")).expect_err("must refuse");
        assert_eq!(e.key, "SQUEEZEFS_X");
        assert_eq!(e.raw, "yess");
        let msg = e.to_string();
        assert!(msg.contains("SQUEEZEFS_X='yess'"), "{msg}");
        assert!(msg.contains("1/true/yes/on"), "{msg}");
    }

    #[test]
    fn integers_parse_trim_and_refuse_junk() {
        assert_eq!(parse_int::<u64>("K", Some(" 42 ")), Ok(Some(42)));
        assert_eq!(parse_int::<u64>("K", None), Ok(None));
        assert!(parse_int::<u64>("K", Some("4 2")).is_err());
        assert!(parse_int::<u64>("K", Some("-1")).is_err(), "u64 rejects -1");
        assert_eq!(parse_int::<i64>("K", Some("-1")), Ok(Some(-1)));
        let e = parse_int::<u32>("SQUEEZEFS_N", Some("1e6")).expect_err("must refuse");
        assert!(e.to_string().contains("SQUEEZEFS_N='1e6'"), "{e}");
    }

    #[test]
    fn out_of_range_is_refused_not_clamped() {
        assert_eq!(parse_int_in::<u32>("K", Some("8"), 1, 32), Ok(Some(8)));
        assert_eq!(parse_int_in::<u32>("K", Some("1"), 1, 32), Ok(Some(1)));
        assert_eq!(parse_int_in::<u32>("K", Some("32"), 1, 32), Ok(Some(32)));
        let e = parse_int_in::<u32>("SQUEEZEFS_D", Some("64"), 1, 32).expect_err("must refuse");
        assert!(e.to_string().contains("1..=32"), "{e}");
        assert!(parse_int_in::<u32>("K", Some("0"), 1, 32).is_err());
    }

    #[test]
    fn enums_match_case_insensitively_and_report_the_set() {
        let allowed = ["always", "second-touch", "never"];
        assert_eq!(
            parse_enum("K", Some("SECOND-TOUCH"), &allowed),
            Ok(Some("second-touch")),
            "the canonical spelling comes back"
        );
        assert_eq!(parse_enum("K", None, &allowed), Ok(None));
        let e = parse_enum("SQUEEZEFS_E", Some("sometimes"), &allowed).expect_err("must refuse");
        assert!(e.to_string().contains("always|second-touch|never"), "{e}");
    }

    #[test]
    fn free_form_values_are_absent_or_trimmed() {
        assert_eq!(parse_str(Some(" /run/x ")), Some("/run/x"));
        assert_eq!(parse_str(Some("")), None);
        assert_eq!(parse_str(None), None);
    }
}
