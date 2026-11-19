//! `regex`: regular expressions, on the `regex` crate (linear time: no backtracking blow-ups,
//! no look-around, no back-references).
//!
//! `regex.compile(pattern) -> Result[Regex]`; a `Regex` is a host object (a reference value:
//! copies of the variable share it) with `is_match`, `find`, `find_all`, `captures`, `replace`,
//! `replace_all` and `split`. The pattern syntax is the one documented by the Rust `regex`
//! crate. No capability is needed: nothing here reaches outside the program.
//!
//! Offsets are **byte offsets** into the UTF-8 text, the same unit as a string slice
//! (`s[m.start..m.end] == m.text`), never character counts.

use colibri_runtime::{
    Cx, Error, HostType, IntoColibri, TypeBuilder, TypeDesc, TypeRegistry, Value, Vm,
};

use crate::plain;

/// A match: `text` is `s[start..end]`.
struct Match {
    text: String,
    start: i64,
    end: i64,
}

impl Match {
    fn of(m: regex::Match<'_>) -> Match {
        Match {
            text: m.as_str().to_string(),
            start: m.start() as i64,
            end: m.end() as i64,
        }
    }
}

impl IntoColibri for Match {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        let int = <i64 as IntoColibri>::ty(reg);
        let text = <String as IntoColibri>::ty(reg);
        reg.define_struct(
            "Match",
            vec![("text", text), ("start", int.clone()), ("end", int)],
        )
    }

    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        let fields = vec![
            self.text.into_colibri(cx),
            self.start.into_colibri(cx),
            self.end.into_colibri(cx),
        ];
        cx.new_struct("Match", fields)
    }
}

/// A compiled pattern (`regex.Regex` in scripts).
struct Regex(regex::Regex);

/// The groups of one match (`regex.Captures`): group 0 is the whole match.
struct Captures {
    groups: Vec<Option<Match>>,
    names: Vec<Option<String>>,
}

impl Captures {
    fn named(&self, name: &str) -> Option<&Match> {
        let i = self.names.iter().position(|n| n.as_deref() == Some(name))?;
        self.groups[i].as_ref()
    }
}

impl HostType for Captures {
    const NAME: &'static str = "Captures";

    fn describe(d: &mut TypeBuilder<'_, Self>) {
        // The number of groups, group 0 (the whole match) included.
        d.method("len", [], |this: &Captures| this.groups.len() as i64);
        // The text of group `index`; `none` when the index is out of range or the group did
        // not take part in the match.
        d.method("get", ["index"], |this: &Captures, index: i64| {
            usize::try_from(index)
                .ok()
                .and_then(|i| this.groups.get(i))
                .and_then(|g| g.as_ref())
                .map(|m| m.text.clone())
        });
        d.method("named", ["name"], |this: &Captures, name: String| {
            this.named(&name).map(|m| m.text.clone())
        });
        // Like `get`/`named`, with the offsets of the group.
        d.method("span", ["index"], |this: &Captures, index: i64| {
            usize::try_from(index)
                .ok()
                .and_then(|i| this.groups.get(i))
                .and_then(|g| g.as_ref())
                .map(|m| Match {
                    text: m.text.clone(),
                    start: m.start,
                    end: m.end,
                })
        });
        d.method("named_span", ["name"], |this: &Captures, name: String| {
            this.named(&name).map(|m| Match {
                text: m.text.clone(),
                start: m.start,
                end: m.end,
            })
        });
    }
}

impl HostType for Regex {
    const NAME: &'static str = "Regex";

    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.method("is_match", ["s"], |this: &Regex, s: String| {
            this.0.is_match(&s)
        });
        d.method("find", ["s"], |this: &Regex, s: String| {
            this.0.find(&s).map(Match::of)
        });
        d.method("find_all", ["s"], |this: &Regex, s: String| {
            this.0.find_iter(&s).map(Match::of).collect::<Vec<_>>()
        });
        d.method("captures", ["s"], |this: &Regex, s: String| {
            let caps = this.0.captures(&s)?;
            Some(Captures {
                groups: caps.iter().map(|g| g.map(Match::of)).collect(),
                names: this
                    .0
                    .capture_names()
                    .map(|n| n.map(str::to_string))
                    .collect(),
            })
        });
        // `$1`, `${1}`, `$name` and `${name}` in `replacement` are the groups; `$$` is a dollar
        // sign. (`$1a` reads as the group named `1a`: write `${1}a`.)
        d.method(
            "replace",
            ["s", "replacement"],
            |this: &Regex, s: String, replacement: String| {
                this.0.replace(&s, replacement.as_str()).into_owned()
            },
        );
        d.method(
            "replace_all",
            ["s", "replacement"],
            |this: &Regex, s: String, replacement: String| {
                this.0.replace_all(&s, replacement.as_str()).into_owned()
            },
        );
        d.method("split", ["s"], |this: &Regex, s: String| {
            this.0.split(&s).map(str::to_string).collect::<Vec<_>>()
        });
        d.method("pattern", [], |this: &Regex| this.0.as_str().to_string());
    }
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("regex");
    m.register_type::<Captures>()?;
    m.register_type::<Regex>()?;
    plain(
        m.register_fn_named("compile", ["pattern"], |pattern: String| {
            regex::RegexBuilder::new(&pattern)
                .size_limit(10 << 20)
                .build()
                .map(Regex)
                .map_err(|e| e.to_string())
        }),
    )?;
    Ok(())
}
