//! `evaluate`: the expressions the adapter accepts are paths into the values a frame can see,
//! `name.field[2]`. Nothing else is evaluated (no program code runs while a debugger looks).

/// One step of a path.
#[derive(Debug, PartialEq, Eq)]
pub enum Seg {
    /// A variable, a field, or a tuple position (`.0`).
    Name(String),
    /// What is between the brackets: a list position or a map key as the debugger shows it.
    Index(String),
}

/// Parses `name.field[2]["key"].0`.
pub fn parse(text: &str) -> Result<Vec<Seg>, String> {
    let refusal = || {
        format!(
            "cannot evaluate `{}`: only variable paths such as `name.field[2]` are supported, \
             not expressions",
            text.trim()
        )
    };
    let chars: Vec<char> = text.trim().chars().collect();
    let ident_start = |c: char| c.is_alphabetic() || c == '_';
    let ident_part = |c: char| c.is_alphanumeric() || c == '_';
    let mut segs = Vec::new();
    let mut i = 0;
    if !chars.first().is_some_and(|c| ident_start(*c)) {
        return Err(refusal());
    }
    while i < chars.len() {
        let start = i;
        match chars[i] {
            '.' if i > 0 => {
                i += 1;
                let first = i;
                if i < chars.len() && ident_start(chars[i]) {
                    while i < chars.len() && ident_part(chars[i]) {
                        i += 1;
                    }
                } else {
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i == first {
                    return Err(refusal());
                }
                segs.push(Seg::Name(chars[first..i].iter().collect()));
            }
            '[' => {
                i += 1;
                let first = i;
                if chars.get(i) == Some(&'"') {
                    i += 1;
                    while i < chars.len() && chars[i] != '"' {
                        i += if chars[i] == '\\' { 2 } else { 1 };
                    }
                    i += 1;
                    while chars.get(i) == Some(&' ') {
                        i += 1;
                    }
                    if chars.get(i) != Some(&']') {
                        return Err(refusal());
                    }
                    let inner: String = chars[first..i].iter().collect();
                    segs.push(Seg::Index(inner.trim_end().to_string()));
                } else {
                    while i < chars.len() && chars[i] != ']' {
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err(refusal());
                    }
                    let inner: String = chars[first..i].iter().collect();
                    let inner = inner.trim();
                    // A position or a key that is a plain literal; `a[i + 1]` is an expression.
                    let literal = !inner.is_empty()
                        && inner
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                    if !literal {
                        return Err(refusal());
                    }
                    segs.push(Seg::Index(inner.to_string()));
                }
                i += 1;
            }
            c if i == 0 && ident_start(c) => {
                while i < chars.len() && ident_part(chars[i]) {
                    i += 1;
                }
                segs.push(Seg::Name(chars[start..i].iter().collect()));
            }
            _ => return Err(refusal()),
        }
    }
    Ok(segs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Seg {
        Seg::Name(s.into())
    }
    fn index(s: &str) -> Seg {
        Seg::Index(s.into())
    }

    #[test]
    fn paths_parse() {
        assert_eq!(parse("p").unwrap(), [name("p")]);
        assert_eq!(
            parse(" a.b[2].0 ").unwrap(),
            [name("a"), name("b"), index("2"), name("0")]
        );
        assert_eq!(
            parse(r#"m["k ]"]"#).unwrap(),
            [name("m"), index(r#""k ]""#)]
        );
    }

    #[test]
    fn expressions_are_refused() {
        for text in [
            "1 + 2", "a + b", "f(x)", "a[i + 1]", "a.", "a..b", "", "-x", "a b", "\"s\"",
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains("only variable paths"), "{text}: {e}");
        }
    }
}
