//! `args`: a small command-line parser over the arguments of the script (`os.args()`).
//!
//! The grammar, which is all of it:
//!
//! - `--` ends the options: everything after it is positional, even `--x` and `-v`.
//! - `--name` is a flag, `--name=value` and `--name value` give `name` a value. `-v` is the
//!   short form of a flag, `-vq` is `-v -q`, and `-o value` or `-o=value` gives `o` a value.
//! - `-` alone and a negative number (`-5`) are positional.
//! - The last occurrence of an option wins.
//!
//! Which words are values and which are positional cannot be known without a declaration, so
//! `value("out")` accepts `--out file`, but `positional()` only knows `--out=file`; with the
//! space form use `positional_with(["out"])` to name the options that take the next word.

use colibri_runtime::{Ctx, Error, Vm};

use crate::config::State;
use crate::plain;

#[derive(Debug, PartialEq)]
enum Item<'a> {
    /// `--name` or `-n` (one letter of a group).
    Flag(&'a str),
    /// `--name=value`, or a word taken as the value of the option before it.
    Value(&'a str, &'a str),
    Positional(&'a str),
}

fn is_option(word: &str) -> bool {
    let bytes = word.as_bytes();
    bytes.len() > 1
        && bytes[0] == b'-'
        && !(bytes[1].is_ascii_digit() || (bytes[1] == b'.' && word.len() > 2))
}

/// Reads the words. `takes_value` names the options that take the next word as their value
/// when they are written without `=`.
fn parse<'a>(words: &'a [String], takes_value: &dyn Fn(&str) -> bool) -> Vec<Item<'a>> {
    let mut items = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let word = words[i].as_str();
        i += 1;
        if word == "--" {
            items.extend(words[i..].iter().map(|w| Item::Positional(w)));
            break;
        }
        if !is_option(word) {
            items.push(Item::Positional(word));
            continue;
        }
        if let Some(long) = word.strip_prefix("--") {
            match long.split_once('=') {
                Some((name, value)) => items.push(Item::Value(name, value)),
                None if takes_value(long) && i < words.len() && words[i] != "--" => {
                    items.push(Item::Value(long, &words[i]));
                    i += 1;
                }
                None => items.push(Item::Flag(long)),
            }
            continue;
        }
        // One dash: `-o=value`, `-o value`, or a group of one-letter flags.
        let short = &word[1..];
        if let Some((name, value)) = short.split_once('=') {
            items.push(Item::Value(name, value));
        } else if short.chars().count() == 1
            && takes_value(short)
            && i < words.len()
            && words[i] != "--"
        {
            items.push(Item::Value(short, &words[i]));
            i += 1;
        } else {
            for (at, c) in short.char_indices() {
                items.push(Item::Flag(&short[at..at + c.len_utf8()]));
            }
        }
    }
    items
}

fn flag(words: &[String], name: &str) -> bool {
    parse(words, &|_| false)
        .iter()
        .any(|item| matches!(item, Item::Flag(n) if *n == name))
}

fn value(words: &[String], name: &str) -> Option<String> {
    parse(words, &|n| n == name)
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::Value(n, v) if *n == name => Some((*v).to_string()),
            _ => None,
        })
}

fn positional(words: &[String], with_values: &[String]) -> Vec<String> {
    parse(words, &|n| with_values.iter().any(|v| v == n))
        .into_iter()
        .filter_map(|item| match item {
            Item::Positional(p) => Some(p.to_string()),
            _ => None,
        })
        .collect()
}

fn script_args(cx: &mut Ctx) -> Vec<String> {
    cx.state_mut::<State>()
        .map(|s| s.args.clone())
        .unwrap_or_default()
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("args");
    // Like `os.args()`: what the host handed the script, in order, nothing removed.
    plain(m.register_fn_named("list", [], script_args))?;
    plain(
        m.register_fn_named("flag", ["name"], |cx: &mut Ctx, name: String| {
            flag(&script_args(cx), &name)
        }),
    )?;
    plain(
        m.register_fn_named("value", ["name"], |cx: &mut Ctx, name: String| {
            value(&script_args(cx), &name)
        }),
    )?;
    plain(m.register_fn_named("positional", [], |cx: &mut Ctx| {
        positional(&script_args(cx), &[])
    }))?;
    plain(m.register_fn_named(
        "positional_with",
        ["value_options"],
        |cx: &mut Ctx, value_options: Vec<String>| positional(&script_args(cx), &value_options),
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_long_short_and_grouped() {
        let a = w(&["--verbose", "-qx", "file"]);
        assert!(flag(&a, "verbose") && flag(&a, "q") && flag(&a, "x"));
        assert!(!flag(&a, "file") && !flag(&a, "v"));
    }

    #[test]
    fn values_in_both_forms_and_the_last_wins() {
        let a = w(&["--out=a", "--out", "b", "-n", "3", "-m=4"]);
        assert_eq!(value(&a, "out").as_deref(), Some("b"));
        assert_eq!(value(&a, "n").as_deref(), Some("3"));
        assert_eq!(value(&a, "m").as_deref(), Some("4"));
        assert_eq!(value(&a, "x"), None);
        assert_eq!(value(&w(&["--out"]), "out"), None);
    }

    #[test]
    fn the_terminator_and_negative_numbers() {
        let a = w(&["x", "-5", "-", "--", "--flag", "-v"]);
        assert_eq!(positional(&a, &[]), ["x", "-5", "-", "--flag", "-v"]);
        assert!(!flag(&a, "flag") && !flag(&a, "v"));
        assert_eq!(value(&w(&["--out", "--", "f"]), "out"), None);
    }

    #[test]
    fn positional_with_value_options() {
        let a = w(&["--out", "f", "in", "--k=v", "more"]);
        assert_eq!(positional(&a, &[]), ["f", "in", "more"]);
        assert_eq!(positional(&a, &w(&["out"])), ["in", "more"]);
    }
}
