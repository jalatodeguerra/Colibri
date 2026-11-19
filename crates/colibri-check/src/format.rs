//! Format specs of string interpolation (DESIGN §12): `{x:.2}`, `{n:>8}`, `{id:x}`.
//!
//! The grammar is a Python-like subset: `[[fill]align][0][width][.precision][type]` with
//! `align` one of `<` `>` `^`, and `type` one of `x` `X` `b` `o` (integers only). Unlike
//! Python, `.N` on a float means N digits after the point (Python's bare `.N` means N
//! significant digits), because that is what `{dt:.2}` is almost always meant to do.

use crate::hir::{Align, FormatSpec, FormatType};
use crate::types::Ty;

/// The widest field and the most digits a spec may ask for, so a typo cannot allocate
/// gigabytes.
const MAX_WIDTH: usize = 10_000;

/// Parses the text after the `:` of a hole. `Err` is a message for the user.
pub fn parse_spec(text: &str) -> Result<FormatSpec, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut pos = 0;
    let mut spec = FormatSpec {
        fill: ' ',
        align: None,
        zero: false,
        width: 0,
        precision: None,
        ty: None,
    };
    let align_of = |c: char| match c {
        '<' => Some(Align::Left),
        '>' => Some(Align::Right),
        '^' => Some(Align::Center),
        _ => None,
    };
    if chars.len() >= 2
        && let Some(align) = align_of(chars[1])
    {
        spec.fill = chars[0];
        spec.align = Some(align);
        pos = 2;
    } else if let Some(align) = chars.first().and_then(|&c| align_of(c)) {
        spec.align = Some(align);
        pos = 1;
    }
    if chars.get(pos) == Some(&'0') {
        spec.zero = true;
        pos += 1;
    }
    let digits = |pos: &mut usize| -> Result<Option<usize>, String> {
        let start = *pos;
        while chars.get(*pos).is_some_and(char::is_ascii_digit) {
            *pos += 1;
        }
        if start == *pos {
            return Ok(None);
        }
        let text: String = chars[start..*pos].iter().collect();
        match text.parse::<usize>() {
            Ok(n) if n <= MAX_WIDTH => Ok(Some(n)),
            _ => Err(format!(
                "the width or precision is too large (at most {MAX_WIDTH})"
            )),
        }
    };
    spec.width = digits(&mut pos)?.unwrap_or(0);
    if chars.get(pos) == Some(&'.') {
        pos += 1;
        match digits(&mut pos)? {
            Some(n) => spec.precision = Some(n),
            None => return Err("expected digits after `.` in the precision".into()),
        }
    }
    if let Some(&c) = chars.get(pos) {
        spec.ty = Some(match c {
            'x' => FormatType::LowerHex,
            'X' => FormatType::UpperHex,
            'b' => FormatType::Binary,
            'o' => FormatType::Octal,
            _ => return Err(format!("unknown format type `{c}`")),
        });
        pos += 1;
    }
    if pos < chars.len() {
        let rest: String = chars[pos..].iter().collect();
        return Err(format!("unexpected `{rest}` at the end of the spec"));
    }
    Ok(spec)
}

/// Whether `spec` makes sense for a value of type `ty`. `Err` is a message for the user.
pub fn validate(spec: &FormatSpec, ty: &Ty) -> Result<(), String> {
    let numeric = matches!(ty, Ty::Int(_) | Ty::Float | Ty::F32);
    let textual = matches!(ty, Ty::Str | Ty::Char | Ty::Bool);
    if !numeric && !textual {
        return Err(format!("`{ty}` values cannot take a format spec"));
    }
    if spec.zero && !numeric {
        return Err(format!("the `0` flag only applies to numbers, not `{ty}`"));
    }
    if let Some(letter) = spec.ty {
        let c = match letter {
            FormatType::LowerHex => 'x',
            FormatType::UpperHex => 'X',
            FormatType::Binary => 'b',
            FormatType::Octal => 'o',
        };
        if !matches!(ty, Ty::Int(_)) {
            return Err(format!(
                "format type `{c}` only applies to integers, not `{ty}`"
            ));
        }
    }
    if spec.precision.is_some() && matches!(ty, Ty::Int(_) | Ty::Char | Ty::Bool) {
        let hint = if matches!(ty, Ty::Int(_)) {
            " (a precision applies to floats and strings)"
        } else {
            ""
        };
        return Err(format!("a precision cannot be used with `{ty}`{hint}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> FormatSpec {
        parse_spec(s).expect("valid spec")
    }

    #[test]
    fn parses_the_documented_examples() {
        let p = parse(".2");
        assert_eq!((p.precision, p.width, p.align), (Some(2), 0, None));
        let n = parse(">8");
        assert_eq!((n.align, n.width, n.fill), (Some(Align::Right), 8, ' '));
        assert_eq!(parse("x").ty, Some(FormatType::LowerHex));
    }

    #[test]
    fn fill_and_align() {
        let s = parse("*^9");
        assert_eq!((s.fill, s.align, s.width), ('*', Some(Align::Center), 9));
        let s = parse("<5");
        assert_eq!((s.fill, s.align), (' ', Some(Align::Left)));
        // A fill that is itself an align character.
        let s = parse("<>4");
        assert_eq!((s.fill, s.align, s.width), ('<', Some(Align::Right), 4));
    }

    #[test]
    fn zero_flag_width_precision_type() {
        let s = parse("08.3");
        assert!(s.zero);
        assert_eq!((s.width, s.precision), (8, Some(3)));
        let s = parse(">x");
        assert_eq!(
            (s.align, s.ty),
            (Some(Align::Right), Some(FormatType::LowerHex))
        );
        for (text, ty) in [
            ("X", FormatType::UpperHex),
            ("b", FormatType::Binary),
            ("o", FormatType::Octal),
        ] {
            assert_eq!(parse(text).ty, Some(ty));
        }
    }

    #[test]
    fn rejects_invalid_specs() {
        assert!(parse_spec("q").unwrap_err().contains("unknown format type"));
        assert!(parse_spec(".").unwrap_err().contains("precision"));
        assert!(parse_spec("5xx").unwrap_err().contains("unexpected"));
        assert!(parse_spec("99999999999999999999").is_err());
        assert!(parse_spec("20000").unwrap_err().contains("too large"));
    }

    #[test]
    fn validation_by_type() {
        let int = Ty::INT;
        assert!(validate(&parse("x"), &int).is_ok());
        assert!(validate(&parse("x"), &Ty::Float).is_err());
        assert!(validate(&parse(".2"), &Ty::Float).is_ok());
        assert!(validate(&parse(".2"), &int).is_err());
        assert!(validate(&parse(".2"), &Ty::Str).is_ok());
        assert!(validate(&parse("05"), &int).is_ok());
        assert!(validate(&parse("05"), &Ty::Str).is_err());
        assert!(validate(&parse(">4"), &Ty::Bool).is_ok());
        assert!(validate(&parse(">4"), &Ty::Unit).is_err());
    }
}
