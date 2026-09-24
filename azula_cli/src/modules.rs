//! Modules: `import "geometry.azl" as geo`.
//!
//! A file imported with a name is a module. Its top-level items are renamed
//! `geo__Point` (throughout the file) so they don't clash with other files',
//! and in the importing file `geo::Point` refers to them. Items are private to
//! their module unless declared `pub`. Plain `import "file.azl"` shares one
//! namespace, as before.

use std::collections::{HashMap, HashSet};

/// What a module declares
#[derive(Clone, Debug, Default)]
pub struct ModuleInfo {
    /// The prefix its items are renamed with (`geo__`)
    pub prefix: String,
    pub items: HashSet<String>,
    pub public: HashSet<String>,
    pub file: String,
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Ident,
    Str,
    Char,
    Comment,
    Space,
    ColonColon,
    Punct(char),
}

#[derive(Clone, Debug)]
struct Tok {
    kind: Kind,
    start: usize,
    end: usize,
}

/// Where a `${` at `start` ends (just past its `}`)
fn interpolation_end(b: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    let mut depth = 1;
    while i < b.len() && depth > 0 {
        match b[i] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

fn scan(text: &str) -> Vec<Tok> {
    let b = text.as_bytes();
    let mut toks = vec![];
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            Kind::Comment
        } else if c.is_ascii_whitespace() {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            Kind::Space
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            Kind::Ident
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            Kind::Punct('0')
        } else if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 2;
                } else if b[i] == b'$' && b.get(i + 1) == Some(&b'{') {
                    i = interpolation_end(b, i);
                } else {
                    i += 1;
                }
            }
            i += 1;
            Kind::Str
        } else if c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != b'\'' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            Kind::Char
        } else if c == b':' && b.get(i + 1) == Some(&b':') {
            i += 2;
            Kind::ColonColon
        } else {
            i += 1;
            Kind::Punct(c as char)
        };
        toks.push(Tok { kind, start, end: i.min(b.len()) });
    }
    toks
}

fn significant(toks: &[Tok]) -> Vec<usize> {
    (0..toks.len()).filter(|&i| !matches!(toks[i].kind, Kind::Space | Kind::Comment)).collect()
}

/// The names of the top-level items `text` declares, and which are `pub`
pub fn collect_items(text: &str) -> (HashSet<String>, HashSet<String>) {
    let toks = scan(text);
    let sig = significant(&toks);
    let word = |i: usize| &text[toks[sig[i]].start..toks[sig[i]].end];
    let mut items = HashSet::new();
    let mut public = HashSet::new();
    let mut depth = 0;
    for k in 0..sig.len() {
        match toks[sig[k]].kind {
            Kind::Punct('{') => depth += 1,
            Kind::Punct('}') => depth -= 1,
            Kind::Ident if depth == 0 => {
                let w = word(k);
                if matches!(w, "func" | "struct" | "enum" | "interface" | "type" | "const") && k + 1 < sig.len() {
                    if toks[sig[k + 1]].kind == Kind::Ident {
                        let name = word(k + 1).to_string();
                        // `pub`, possibly before `extern`
                        let mut j = k;
                        while j > 0 && matches!(word(j - 1), "extern" | "varargs") {
                            j -= 1;
                        }
                        if j > 0 && word(j - 1) == "pub" {
                            public.insert(name.clone());
                        }
                        items.insert(name);
                    }
                }
            }
            _ => {}
        }
    }
    (items, public)
}

/// Rewrite a file's text: rename its own items (if it's a module with
/// `own` items), and turn `alias::name` references to modules into their
/// renamed items. Errors are (byte offset, message).
pub fn rewrite(
    text: &str,
    own: Option<&ModuleInfo>,
    aliases: &HashMap<String, ModuleInfo>,
) -> Result<String, (usize, String)> {
    let toks = scan(text);
    let sig = significant(&toks);
    let mut out = String::new();
    let mut last = 0;
    let word = |t: &Tok| &text[t.start..t.end];
    let mut depth = 0;
    let mut enum_pending = false;
    let mut enum_depths: Vec<i32> = vec![];
    let mut k = 0;
    while k < sig.len() {
        let t = &toks[sig[k]];
        match t.kind {
            Kind::Punct('{') => {
                depth += 1;
                if enum_pending {
                    enum_depths.push(depth);
                    enum_pending = false;
                }
            }
            Kind::Punct('}') => {
                if enum_depths.last() == Some(&depth) {
                    enum_depths.pop();
                }
                depth -= 1;
            }
            Kind::Str => {
                // Rewrite the code inside `${...}`
                let s = word(t);
                if s.contains("${") {
                    let rewritten = rewrite_string(s, own, aliases).map_err(|(o, m)| (t.start + o, m))?;
                    out.push_str(&text[last..t.start]);
                    out.push_str(&rewritten);
                    last = t.end;
                }
            }
            Kind::Ident => {
                let w = word(t);
                if w == "enum" && depth == 0 {
                    enum_pending = true;
                }
                let prev = if k > 0 { Some(&toks[sig[k - 1]]) } else { None };
                let next = sig.get(k + 1).map(|&i| &toks[i]);
                let prev_word = prev.map(|p| word(p)).unwrap_or("");
                let after_member = matches!(prev.map(|p| &p.kind), Some(Kind::Punct('.')) | Some(Kind::ColonColon));
                // `alias::name`
                if let (Some(module), Some(Kind::ColonColon)) = (aliases.get(w), next.map(|n| &n.kind)) {
                    if !after_member {
                        if let Some(name_tok) = sig.get(k + 2).map(|&i| &toks[i]) {
                            if name_tok.kind == Kind::Ident {
                                let name = word(name_tok);
                                if !module.items.contains(name) {
                                    return Err((name_tok.start, format!("`{}` isn't declared in {}", name, module.file)));
                                }
                                if !module.public.contains(name) {
                                    return Err((name_tok.start, format!("`{}` is private to {} (declare it `pub`)", name, module.file)));
                                }
                                out.push_str(&text[last..t.start]);
                                out.push_str(&module.prefix);
                                out.push_str(name);
                                last = name_tok.end;
                                k += 3;
                                continue;
                            }
                        }
                    }
                }
                // The module's own items
                if let Some(module) = own {
                    let declares_member = prev_word == "func" && depth > 0;
                    let is_variant = enum_depths.last() == Some(&depth)
                        && matches!(prev.map(|p| &p.kind), Some(Kind::Punct('{')) | Some(Kind::Punct(';')));
                    let is_label = matches!(next.map(|n| &n.kind), Some(Kind::Punct(':')));
                    if module.items.contains(w) && !after_member && !declares_member && !is_variant && !is_label {
                        out.push_str(&text[last..t.start]);
                        out.push_str(&module.prefix);
                        out.push_str(w);
                        last = t.end;
                    }
                }
            }
            _ => {}
        }
        k += 1;
    }
    out.push_str(&text[last..]);
    Ok(out)
}

/// Rewrite the `${...}` parts of a string literal
fn rewrite_string(s: &str, own: Option<&ModuleInfo>, aliases: &HashMap<String, ModuleInfo>) -> Result<String, (usize, String)> {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut last = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == b'$' && b.get(i + 1) == Some(&b'{') {
            let end = interpolation_end(b, i);
            let code = &s[i + 2..end - 1];
            let rewritten = rewrite(code, own, aliases).map_err(|(o, m)| (i + 2 + o, m))?;
            out.push_str(&s[last..i + 2]);
            out.push_str(&rewritten);
            last = end - 1;
            i = end;
            continue;
        }
        i += 1;
    }
    out.push_str(&s[last..]);
    Ok(out)
}

/// `import "path"` or `import "path" as name` on a line: the path and name
pub fn import_line(line: &str) -> Option<(String, Option<String>)> {
    let rest = line.trim().strip_prefix("import \"")?;
    let end = rest.find('"')?;
    let path = rest[..end].to_string();
    let after = rest[end + 1..].trim().trim_end_matches(';').trim();
    if after.is_empty() {
        return Some((path, None));
    }
    let name = after.strip_prefix("as")?.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((path, Some(name.to_string())))
}
