//! Whether what was typed so far is a whole entry of `colibri repl`, or the statement goes on
//! (ARCHITECTURE §21).

use colibri_syntax::{FileId, TokenKind, tokenize};

/// Whether `text` can be handed to the checker as it is. It cannot while a bracket or a string
/// or block comment is open, or when the last token is one after which a line break does not
/// end a statement (a binary operator, `,`, `.`, `=`, `->`, `fn`, ...; DESIGN §3.2).
/// Blank text is complete (there is nothing to run).
pub fn is_complete(text: &str) -> bool {
    let (tokens, diagnostics) = tokenize(FileId(0), text);
    // Only a `"""` string and a block comment can go on over a line break; an open `"` string
    // is an error the checker will report.
    let open = diagnostics.iter().any(|d| {
        d.message == "unterminated block comment"
            || (d.message == "unterminated string literal"
                && text
                    .get(d.span.start as usize..)
                    .is_some_and(|rest| rest.starts_with("\"\"\"")))
    });
    if open {
        return false;
    }
    let mut depth = 0i32;
    for t in &tokens {
        match t.kind {
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => depth -= 1,
            _ => {}
        }
    }
    if depth > 0 {
        return false;
    }
    let last = tokens.iter().rev().find(|t| {
        !matches!(
            t.kind,
            TokenKind::Semi | TokenKind::Eof | TokenKind::Newline
        )
    });
    let Some(last) = last else { return true };
    use TokenKind::{
        Break, Char, Continue, DocComment, Error, False, Float, Ident, Int, Label, Question,
        RBrace, RBracket, RParen, Return, SelfKw, Str, True, Underscore,
    };
    matches!(
        &last.kind,
        Ident(_)
            | Underscore
            | Int(_)
            | Float(_)
            | Char(_)
            | Str(_)
            | Label(_)
            | True
            | False
            | TokenKind::None
            | SelfKw
            | RParen
            | RBracket
            | RBrace
            | Return
            | Break
            | Continue
            | Question
            | Error
            | DocComment(_)
    )
}

#[cfg(test)]
mod tests {
    use super::is_complete;

    #[test]
    fn simple_entries_are_complete() {
        for text in [
            "",
            "   ",
            "1 + 2",
            "let x = 5",
            "print(\"hi\")",
            "x = 3;",
            "// note",
        ] {
            assert!(is_complete(text), "{text:?}");
        }
    }

    #[test]
    fn open_brackets_continue() {
        for text in ["fn f() {", "foo(1,", "let xs = [1,", "if x {\n  y()", "{"] {
            assert!(!is_complete(text), "{text:?}");
        }
        assert!(is_complete("fn f() {\n  1\n}"));
        assert!(is_complete("foo(\n 1,\n 2\n)"));
    }

    #[test]
    fn open_strings_and_comments_continue() {
        assert!(is_complete("let s = \"abc"));
        assert!(!is_complete("let s = \"\"\"abc\nmore"));
        assert!(is_complete("let s = \"\"\"abc\nmore\"\"\""));
        assert!(!is_complete("/* open"));
        assert!(is_complete("/* closed */ 1"));
    }

    #[test]
    fn trailing_operators_continue() {
        for text in ["1 +", "x =", "a.", "let x:", "fn f() ->", "a and", "x ="] {
            assert!(!is_complete(text), "{text:?}");
        }
        assert!(is_complete("a.b"));
        assert!(is_complete("x?"));
    }

    #[test]
    fn closed_blocks_are_complete() {
        assert!(is_complete("struct P { x: int }"));
        assert!(is_complete("for i in 0..3 {\n print(i)\n}"));
    }
}
