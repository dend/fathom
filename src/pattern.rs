//! Patterns that pick objects by key, or local files by path: a path named in
//! full, a glob such as `photos/**/*.png`, or a regular expression.

use regex::{Regex, RegexBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    /// Plain paths and globs.
    Glob,
    Regex,
}

#[derive(Debug, Clone)]
pub struct Pattern {
    /// What was typed, with `/` between folders.
    pub text: String,
    matcher: Matcher,
    /// Every key the pattern can match starts with this, so a listing can
    /// start there instead of at the top of the bucket.
    pub prefix: String,
    /// The fixed folders the pattern starts with (empty, or ending in `/`).
    /// Copies keep the part of the path after it.
    pub base: String,
}

#[derive(Debug, Clone)]
enum Matcher {
    /// A key or folder named in full, with no wildcards.
    Path,
    Regex(Regex),
}

const GLOB_CHARS: [char; 5] = ['*', '?', '[', '{', '\\'];

impl Pattern {
    /// A pattern for object keys. A backslash makes the next character literal.
    pub fn new(text: &str, syntax: Syntax) -> Result<Self, String> {
        Self::build(text.to_string(), syntax, true, false)
    }

    /// A pattern for paths on this computer. On Windows a backslash separates
    /// folders, as it does there, and globs ignore case like the file system.
    pub fn local(text: &str, syntax: Syntax) -> Result<Self, String> {
        if cfg!(windows) && syntax == Syntax::Glob {
            Self::build(text.replace('\\', "/"), syntax, false, true)
        } else {
            Self::build(text.to_string(), syntax, true, false)
        }
    }

    fn build(
        text: String,
        syntax: Syntax,
        escapes: bool,
        ignore_case: bool,
    ) -> Result<Self, String> {
        if text.is_empty() {
            return Err("A pattern can't be empty".into());
        }
        let wildcards = |c: char| GLOB_CHARS.contains(&c) && (c != '\\' || escapes);
        let (source, prefix) = match syntax {
            Syntax::Glob if !text.contains(wildcards) => {
                let base = crate::format::parent_prefix(&text).to_string();
                return Ok(Self {
                    prefix: text.clone(),
                    matcher: Matcher::Path,
                    base,
                    text,
                });
            }
            Syntax::Glob => {
                let end = text.find(wildcards).unwrap_or(text.len());
                (translate(&text, escapes), text[..end].to_string())
            }
            Syntax::Regex => (text.clone(), regex_prefix(&text)),
        };
        let regex = RegexBuilder::new(&source)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|e| format!("{text} is not a valid pattern: {e}"))?;
        let base = match prefix.rfind('/') {
            Some(index) => prefix[..=index].to_string(),
            None => String::new(),
        };
        Ok(Self {
            text,
            matcher: Matcher::Regex(regex),
            prefix: if ignore_case { String::new() } else { prefix },
            base,
        })
    }

    pub fn matches(&self, key: &str) -> bool {
        match &self.matcher {
            Matcher::Path if self.text.ends_with('/') => key.starts_with(&self.text),
            Matcher::Path => key
                .strip_prefix(&self.text)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/')),
            Matcher::Regex(regex) => regex.is_match(key),
        }
    }

    /// A key or folder named in full. Without a trailing slash it names
    /// that key and the folder of the same name, as a path on disk would.
    pub fn is_path(&self) -> bool {
        matches!(self.matcher, Matcher::Path)
    }

    /// The part of a matching key after the pattern's fixed folders.
    pub fn relative<'a>(&self, key: &'a str) -> &'a str {
        key.strip_prefix(&self.base).unwrap_or(key)
    }
}

/// Translates a glob into an anchored regular expression. `*` and `?` stay
/// within one folder name, `**` crosses folders, `[...]` is a set of
/// characters (`[!...]` the others) and `{a,b}` matches either side.
fn translate(glob: &str, escapes: bool) -> String {
    let chars: Vec<char> = glob.chars().collect();
    let mut out = String::from("^");
    translate_into(&chars, escapes, &mut out);
    out.push('$');
    out
}

fn translate_into(chars: &[char], escapes: bool, out: &mut String) {
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if escapes && i + 1 < chars.len() => {
                push_literal(out, chars[i + 1]);
                i += 2;
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                let mut end = i + 2;
                while chars.get(end) == Some(&'*') {
                    end += 1;
                }
                // `a/**/b` also matches `a/b`.
                let whole_segment = i == 0 || chars[i - 1] == '/';
                if whole_segment && chars.get(end) == Some(&'/') {
                    out.push_str("(?:.*/)?");
                    end += 1;
                } else {
                    out.push_str(".*");
                }
                i = end;
            }
            '*' => {
                out.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            '[' => match class(chars, i, escapes) {
                Some((class, next)) => {
                    out.push_str(&class);
                    i = next;
                }
                None => {
                    push_literal(out, '[');
                    i += 1;
                }
            },
            '{' => match alternatives(chars, i, escapes) {
                Some((options, next)) => {
                    out.push_str("(?:");
                    for (index, option) in options.iter().enumerate() {
                        if index > 0 {
                            out.push('|');
                        }
                        translate_into(option, escapes, out);
                    }
                    out.push(')');
                    i = next;
                }
                None => {
                    push_literal(out, '{');
                    i += 1;
                }
            },
            c => {
                push_literal(out, c);
                i += 1;
            }
        }
    }
}

fn push_literal(out: &mut String, c: char) {
    out.push_str(&regex::escape(c.encode_utf8(&mut [0; 4])));
}

/// The set of characters starting at `chars[start]` (a `[`) as a regex class,
/// and the index after it. `None` when it is never closed, so the `[` is literal.
fn class(chars: &[char], start: usize, escapes: bool) -> Option<(String, usize)> {
    let mut i = start + 1;
    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    // (character, typed as a literal)
    let mut members: Vec<(char, bool)> = Vec::new();
    loop {
        let c = *chars.get(i)?;
        match c {
            // A `]` right at the start is a member, not the end.
            ']' if !members.is_empty() => break,
            '\\' if escapes && i + 1 < chars.len() => {
                members.push((chars[i + 1], true));
                i += 2;
                continue;
            }
            _ => members.push((c, false)),
        }
        i += 1;
    }
    let mut out = String::from(if negated { "[^/" } else { "[" });
    for (index, &(c, literal)) in members.iter().enumerate() {
        let range = c == '-' && !literal && index > 0 && index + 1 < members.len();
        if range {
            out.push('-');
        } else {
            push_literal(&mut out, c);
        }
    }
    out.push(']');
    Some((out, i + 1))
}

/// The comma-separated options of the `{...}` starting at `chars[start]`, and
/// the index after it. `None` when it is never closed or has no comma, so the
/// braces are literal, as in a shell.
fn alternatives(chars: &[char], start: usize, escapes: bool) -> Option<(Vec<&[char]>, usize)> {
    let mut depth = 0;
    let mut options = Vec::new();
    let mut from = start + 1;
    let mut i = start + 1;
    loop {
        match *chars.get(i)? {
            '\\' if escapes => i += 1,
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            '}' => break,
            ',' if depth == 0 => {
                options.push(&chars[from..i]);
                from = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if options.is_empty() {
        return None;
    }
    options.push(&chars[from..i]);
    Some((options, i + 1))
}

/// The literal text every match of a regex starts with, when it is anchored
/// with `^`. Anything unclear gives a shorter prefix, which only means a
/// longer listing.
fn regex_prefix(regex: &str) -> String {
    let Some(rest) = regex.strip_prefix('^') else {
        return String::new();
    };
    if has_top_level_alternation(rest) {
        return String::new();
    }
    // A quantifier that allows zero repeats makes the character before it optional.
    let optional = |next: Option<&char>| matches!(next, Some('*' | '?' | '{'));
    let mut prefix = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // `a+` needs one `a`, unless something like `a+*` makes it optional.
            '+' => {
                if optional(chars.peek()) {
                    prefix.pop();
                }
                break;
            }
            '.' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' => break,
            '\\' => match chars.next() {
                Some(escaped) if escaped.is_ascii_punctuation() => prefix.push(escaped),
                // `\d`, `\w`, `\x41` and friends: a class or a code, not a literal.
                _ => break,
            },
            c => prefix.push(c),
        }
        if optional(chars.peek()) {
            prefix.pop();
            break;
        }
    }
    prefix
}

/// Whether `regex` has a `|` outside any group, which makes the `^` before it
/// apply to the first option only.
fn has_top_level_alternation(regex: &str) -> bool {
    let mut depth = 0usize;
    let mut chars = regex.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '[' => skip_class(&mut chars),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

/// Skips the rest of a `[...]` class, which may hold nested classes.
fn skip_class(chars: &mut std::str::Chars<'_>) {
    let mut depth = 1;
    let mut first = true;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '^' if first => continue,
            ']' if first => {}
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            }
            _ => {}
        }
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(text: &str) -> Pattern {
        Pattern::new(text, Syntax::Glob).unwrap()
    }

    fn regex(text: &str) -> Pattern {
        Pattern::new(text, Syntax::Regex).unwrap()
    }

    #[test]
    fn paths_name_a_file_or_a_folder() {
        let path = glob("photos/2024");
        assert!(path.is_path());
        assert!(path.matches("photos/2024"));
        assert!(path.matches("photos/2024/reef.png"));
        assert!(!path.matches("photos/2024-old/reef.png"));
        assert!(!path.matches("photos/20"));
        assert_eq!(path.prefix, "photos/2024");
        assert_eq!(path.base, "photos/");
        assert_eq!(path.relative("photos/2024/reef.png"), "2024/reef.png");

        let folder = glob("photos/2024/");
        assert!(folder.matches("photos/2024/reef.png"));
        assert!(folder.matches("photos/2024/"));
        assert!(!folder.matches("photos/2024"));
        assert_eq!(folder.base, "photos/");
    }

    #[test]
    fn single_stars_stay_in_one_folder() {
        let pattern = glob("photos/*.png");
        assert!(pattern.matches("photos/reef.png"));
        assert!(pattern.matches("photos/.png"));
        assert!(!pattern.matches("photos/2024/reef.png"));
        assert!(!pattern.matches("photos/reef.png.bak"));
        assert_eq!(pattern.prefix, "photos/");
        assert_eq!(pattern.base, "photos/");

        let one = glob("logs/app-?.log");
        assert!(one.matches("logs/app-1.log"));
        assert!(!one.matches("logs/app-10.log"));
        assert!(!glob("a?b").matches("a/b"));
    }

    #[test]
    fn double_stars_cross_folders() {
        let all = glob("photos/**");
        assert!(all.matches("photos/a.png"));
        assert!(all.matches("photos/2024/deep/a.png"));
        assert!(!all.matches("photos"));

        let nested = glob("photos/**/*.png");
        assert!(nested.matches("photos/a.png"));
        assert!(nested.matches("photos/2024/a.png"));
        assert!(!nested.matches("photos/2024/a.jpg"));

        let anywhere = glob("**/*.png");
        assert!(anywhere.matches("a.png"));
        assert!(anywhere.matches("x/y/a.png"));
        assert_eq!(anywhere.prefix, "");
        assert_eq!(anywhere.base, "");

        // A partial path: everything whose key starts with it.
        let partial = glob("logs/2024-**");
        assert!(partial.matches("logs/2024-01/app.log"));
        assert!(partial.matches("logs/2024-12"));
        assert!(!partial.matches("logs/2023-12/app.log"));
        assert_eq!(partial.prefix, "logs/2024-");
        assert_eq!(partial.base, "logs/");
        assert_eq!(partial.relative("logs/2024-01/app.log"), "2024-01/app.log");
    }

    #[test]
    fn sets_and_alternatives() {
        let set = glob("img[0-9].png");
        assert!(set.matches("img7.png"));
        assert!(!set.matches("imgx.png"));
        let negated = glob("img[!0-9].png");
        assert!(negated.matches("imgx.png"));
        assert!(!negated.matches("img7.png"));
        assert!(!negated.matches("img/.png"));
        // `]` first is a member; `-` at either end is literal.
        assert!(glob("[]a]").matches("]"));
        assert!(glob("[a-]").matches("-"));
        // Regex class syntax inside a glob set is just characters.
        assert!(glob("[&&~]").matches("&"));

        let either = glob("**/*.{jpg,png}");
        assert!(either.matches("a/b.jpg"));
        assert!(either.matches("b.png"));
        assert!(!either.matches("b.gif"));
        assert!(glob("{logs,tmp}/**").matches("tmp/x"));
        assert!(glob("a{b,{c,d}}e").matches("ade"));

        // Braces without a comma, and unclosed brackets, are plain characters.
        assert!(glob("data{1}.json").matches("data{1}.json"));
        assert!(glob("a[b").matches("a[b"));
        assert!(glob("a{b").matches("a{b"));
    }

    #[test]
    fn backslashes_escape_wildcards_in_keys() {
        let literal = glob("notes/what\\?.txt");
        assert!(literal.matches("notes/what?.txt"));
        assert!(!literal.matches("notes/whatx.txt"));
        assert!(glob("a\\*").matches("a*"));
        // Regex metacharacters in keys are literal in a glob.
        assert!(glob("v1.0 (final)+$/*").matches("v1.0 (final)+$/x"));
        assert!(!glob("v1.0/*").matches("v1x0/x"));
    }

    #[test]
    #[cfg(windows)]
    fn local_globs_follow_windows_conventions() {
        let pattern = Pattern::local("C:\\site\\dist\\*.HTML", Syntax::Glob).unwrap();
        assert!(pattern.matches("C:/site/dist/index.html"));
        assert_eq!(pattern.base, "C:/site/dist/");
        // Case-insensitive matches can't be narrowed by a case-sensitive prefix.
        assert_eq!(pattern.prefix, "");
    }

    #[test]
    fn regexes_match_anywhere_unless_anchored() {
        let loose = regex(r"\.tmp$");
        assert!(loose.matches("a/b/c.tmp"));
        assert!(!loose.matches("a/b/c.tmp.bak"));
        assert_eq!(loose.prefix, "");
        assert_eq!(loose.base, "");

        let anchored = regex(r"^logs/2024-0[1-3]/.*\.log$");
        assert!(anchored.matches("logs/2024-02/app.log"));
        assert!(!anchored.matches("logs/2024-04/app.log"));
        assert_eq!(anchored.prefix, "logs/2024-0");
        assert_eq!(anchored.base, "logs/");

        assert!(Pattern::new("(unclosed", Syntax::Regex).is_err());
        assert!(Pattern::new("", Syntax::Glob).is_err());
    }

    #[test]
    fn regex_prefixes_never_overreach() {
        for (regex, prefix) in [
            ("^photos/", "photos/"),
            (r"^a\.b/c", "a.b/c"),
            ("^abc?", "ab"),
            ("^abc*", "ab"),
            ("^abc+", "abc"),
            ("^abc{0,2}", "ab"),
            (r"^ab\.?", "ab"),
            (r"^ab\d", "ab"),
            ("^été/x", "été/x"),
            ("^a|b", ""),
            ("^a(b|c)", "a"),
            ("^a[|]b", "a"),
            (r"^a\|b", "a|b"),
            ("^[a]|b", ""),
            ("(?i)^abc", ""),
            ("photos/", ""),
        ] {
            assert_eq!(regex_prefix(regex), prefix, "{regex}");
            // Whatever the prefix, every match must start with it.
            let compiled = Regex::new(regex).unwrap();
            for key in [
                "photos/x", "a.b/c", "ab", "abcc", "a", "ab7", "été/x", "b", "a|b", "ac",
            ] {
                if compiled.is_match(key) && regex.starts_with('^') {
                    assert!(key.starts_with(prefix), "{regex} matched {key}");
                }
            }
        }
    }
}
