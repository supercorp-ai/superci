//! containerd's filters (`pkg/filters`): what `List` of snapshots, content and statuses is given to choose with, as
//! containerd parses and matches them, its errors word for word.
//!
//! ```text
//! selectors := selector ("," selector)*
//! selector  := fieldpath (operator value)
//! fieldpath := field ('.' field)*
//! field     := quoted | [A-Za-z] [A-Za-z0-9_]+
//! operator  := "==" | "!=" | "~="
//! value     := quoted | [^\s,]+
//! quoted    := <go string syntax>, or /…/ and |…| for a regular expression's value
//! ```
//!
//! Several filters match when any one does; a filter matches when all of its selectors do; no filters match all.

/// What a filter reads of a thing: a field's value by its path, `None` when it has none. (A missing field reads as
/// empty where its value is compared, as containerd's adaptors give "" with `false`.)
pub trait Adaptor {
    fn field(&self, path: &[String]) -> Option<String>;
}

impl<F: Fn(&[String]) -> Option<String>> Adaptor for F {
    fn field(&self, path: &[String]) -> Option<String> { self(path) }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op { Present, Equal, NotEqual, Matches }

#[derive(Debug)]
struct Selector { path: Vec<String>, op: Op, value: String, re: Option<regex::Regex> }

impl Selector {
    fn matches(&self, a: &dyn Adaptor) -> bool {
        let got = a.field(&self.path);
        match self.op {
            Op::Present => got.is_some(),
            Op::Equal => got.is_some_and(|v| v == self.value),
            Op::NotEqual => got.unwrap_or_default() != self.value,
            // (A value that is no regular expression matches nothing.)
            Op::Matches => self.re.as_ref().is_some_and(|re| re.is_match(&got.unwrap_or_default())),
        }
    }
}

/// Filters as given: any of them matching, each of its selectors matching.
#[derive(Debug, Default)]
pub struct Filters(Vec<Vec<Selector>>);

impl Filters {
    /// containerd's `ParseAll`: each filter, or the first one's error (said as containerd says it, an invalid argument).
    pub fn parse_all<S: AsRef<str>>(filters: &[S]) -> Result<Filters, String> {
        let mut all = vec![];
        for f in filters {
            match parse(f.as_ref()) {
                Ok(s) => all.push(s),
                Err(e) => return Err(format!("{e}: invalid argument")),
            }
        }
        Ok(Filters(all))
    }

    pub fn matches(&self, a: &dyn Adaptor) -> bool {
        self.0.is_empty() || self.0.iter().any(|selectors| selectors.iter().all(|s| s.matches(a)))
    }
}

/// One filter's selectors (containerd's `Parse`): empty matches everything.
fn parse(input: &str) -> Result<Vec<Selector>, String> {
    if input.is_empty() { return Ok(vec![]) }
    let mut p = Parser { input, s: Scanner::new(input) };
    p.selectors().map_err(|e| format!("filters: {e}"))
}

// The scanner's runes are a char's code, or one of these below zero.
const EOF: i32 = -1;
const QUOTED: i32 = -2;
const VALUE: i32 = -3;
const FIELD: i32 = -4;
const SEPARATOR: i32 = -5;
const OPERATOR: i32 = -6;
const ILLEGAL: i32 = -7;

struct Scanner<'a> { input: &'a str, pos: usize, ppos: usize, value: bool, err: String }

impl<'a> Scanner<'a> {
    fn new(input: &'a str) -> Self { Scanner { input, pos: 0, ppos: 0, value: false, err: String::new() } }

    fn next(&mut self) -> i32 {
        if self.pos >= self.input.len() { return EOF }
        self.pos = self.ppos;
        let Some(c) = self.input[self.ppos..].chars().next() else { return EOF };
        self.ppos += c.len_utf8();
        // (Go reads the replacement character as a rune it could not decode.)
        if c == '\u{fffd}' { self.error("rune error"); return ILLEGAL }
        if c == '\0' { self.error("unexpected null"); return ILLEGAL }
        c as i32
    }

    fn peek(&mut self) -> i32 {
        let (pos, ppos) = (self.pos, self.ppos);
        let ch = self.next();
        (self.pos, self.ppos) = (pos, ppos);
        ch
    }

    fn text(&self, from: usize) -> String { self.input[from..self.ppos].to_string() }

    fn scan(&mut self) -> (usize, i32, String) {
        let mut ch = self.next();
        let mut pos = self.pos;
        loop {
            return match ch {
                EOF | ILLEGAL => (self.pos, ch, String::new()),
                _ if is_quote(ch) => {
                    let legal = self.scan_quoted(ch);
                    (pos, if legal { QUOTED } else { ILLEGAL }, self.text(pos))
                }
                _ if is_separator(ch) => { self.value = false; (pos, SEPARATOR, self.text(pos)) }
                _ if is_operator(ch) => { self.scan_operator(); self.value = true; (pos, OPERATOR, self.text(pos)) }
                _ if is_space(ch) => { ch = self.next(); pos = self.pos; continue }
                _ if self.value => { self.scan_value(); self.value = false; (pos, VALUE, self.text(pos)) }
                _ if is_field(ch) => { self.scan_field(); (pos, FIELD, self.text(pos)) }
                _ => (self.pos, ch, String::new()),
            };
        }
    }

    fn scan_field(&mut self) { while is_field(self.peek()) { self.next(); } }
    fn scan_operator(&mut self) { while is_operator(self.peek()) { self.next(); } }
    fn scan_value(&mut self) { while is_value(self.peek()) { self.next(); } }

    fn scan_quoted(&mut self, quote: i32) -> bool {
        let mut illegal = false;
        let mut ch = self.next();
        while ch != quote {
            if ch == '\n' as i32 || ch < 0 { self.error("quoted literal not terminated"); return false }
            if ch == '\\' as i32 {
                let (c, legal) = self.scan_escape(quote);
                ch = c;
                if !legal { illegal = true }
            } else {
                ch = self.next();
            }
        }
        !illegal
    }

    fn scan_escape(&mut self, quote: i32) -> (i32, bool) {
        let ch = self.next();
        match u8::try_from(ch).ok().map(char::from) {
            Some('a' | 'b' | 'f' | 'n' | 'r' | 't' | 'v' | '\\') => (self.next(), true),
            _ if ch == quote => (self.next(), true),
            Some('0'..='7') => self.scan_digits(ch, 8, 3),
            Some('x') => { let c = self.next(); self.scan_digits(c, 16, 2) }
            Some('u') => { let c = self.next(); self.scan_digits(c, 16, 4) }
            Some('U') => { let c = self.next(); self.scan_digits(c, 16, 8) }
            _ => { self.error("illegal escape sequence"); (ch, false) }
        }
    }

    fn scan_digits(&mut self, mut ch: i32, base: i32, mut n: i32) -> (i32, bool) {
        while n > 0 && digit_val(ch) < base { ch = self.next(); n -= 1 }
        if n > 0 { self.error("illegal numeric escape sequence"); return (ch, false) }
        (ch, true)
    }

    fn error(&mut self, msg: &str) { if self.err.is_empty() { self.err = msg.to_string() } }
}

fn as_char(r: i32) -> Option<char> { u32::try_from(r).ok().and_then(char::from_u32) }
fn digit_val(r: i32) -> i32 {
    match as_char(r) { Some(c @ '0'..='9') => c as i32 - '0' as i32, Some(c @ 'a'..='f') => c as i32 - 'a' as i32 + 10, Some(c @ 'A'..='F') => c as i32 - 'A' as i32 + 10, _ => 16 }
}
fn is_field(r: i32) -> bool { as_char(r).is_some_and(|c| c == '_' || c.is_ascii_alphanumeric()) }
fn is_operator(r: i32) -> bool { matches!(as_char(r), Some('=' | '!' | '~')) }
fn is_quote(r: i32) -> bool { matches!(as_char(r), Some('/' | '|' | '"')) }
fn is_separator(r: i32) -> bool { matches!(as_char(r), Some(',' | '.')) }
fn is_space(r: i32) -> bool { as_char(r).is_some_and(char::is_whitespace) }
/// Anything but a comma or a space that is printed (Go's letters, digits, numbers, graphic and punctuation runes; a
/// control character is none of them).
fn is_value(r: i32) -> bool { as_char(r).is_some_and(|c| c != ',' && !c.is_whitespace() && !c.is_control()) }

struct Parser<'a> { input: &'a str, s: Scanner<'a> }

impl Parser<'_> {
    fn selectors(&mut self) -> Result<Vec<Selector>, String> {
        let mut all = vec![self.selector().and_then(crate::faults::said)?];
        loop {
            match self.s.peek() {
                // (A comma scans as a separator.)
                c if c == ',' as i32 => {
                    self.s.scan();
                    all.push(self.selector().and_then(crate::faults::said)?);
                }
                EOF => return Ok(all),
                c => { let at = self.s.ppos; return Err(self.err(at, &format!("unexpected input: {}", as_char(c).unwrap_or('\u{fffd}')))) }
            }
        }
    }

    fn selector(&mut self) -> Result<Selector, String> {
        let path = self.fieldpath().and_then(crate::faults::said)?;
        let next = self.s.peek();
        if next == ',' as i32 || next == EOF { return Ok(Selector { path, op: Op::Present, value: String::new(), re: None }) }
        let op = self.operator().and_then(crate::faults::said)?;
        let value = self.value(op == Op::Matches).and_then(crate::faults::said)?;
        let re = if op == Op::Matches { regex::Regex::new(&value).ok() } else { None };
        Ok(Selector { path, op, value, re })
    }

    fn fieldpath(&mut self) -> Result<Vec<String>, String> {
        let mut fields = vec![self.field().and_then(crate::faults::said)?];
        // (A dot scans as a separator.)
        while self.s.peek() == '.' as i32 {
            self.s.scan();
            fields.push(self.field().and_then(crate::faults::said)?);
        }
        Ok(fields)
    }

    fn field(&mut self) -> Result<String, String> {
        let (pos, tok, text) = self.s.scan();
        match tok {
            FIELD => Ok(text),
            QUOTED => self.unquote(pos, &text, false),
            ILLEGAL => { let e = self.s.err.clone(); Err(self.err(pos, &e)) }
            _ => Err(self.err(pos, "expected field or quoted")),
        }
    }

    fn operator(&mut self) -> Result<Op, String> {
        let (pos, tok, text) = self.s.scan();
        match tok {
            OPERATOR => match text.as_str() {
                "==" => Ok(Op::Equal),
                "!=" => Ok(Op::NotEqual),
                "~=" => Ok(Op::Matches),
                _ => Err(self.err(pos, &format!("unsupported operator {text:?}"))),
            },
            ILLEGAL => { let e = self.s.err.clone(); Err(self.err(pos, &e)) }
            _ => Err(self.err(pos, r#"expected an operator ("=="|"!="|"~=")"#)),
        }
    }

    fn value(&mut self, alt_quotes: bool) -> Result<String, String> {
        let (pos, tok, text) = self.s.scan();
        match tok {
            VALUE | FIELD => Ok(text),
            QUOTED => self.unquote(pos, &text, alt_quotes),
            ILLEGAL => { let e = self.s.err.clone(); Err(self.err(pos, &e)) }
            _ => Err(self.err(pos, "expected value or quoted")),
        }
    }

    fn unquote(&self, pos: usize, s: &str, alt_quotes: bool) -> Result<String, String> {
        if !alt_quotes && !s.starts_with('"') { return Err(self.err(pos, "invalid quote encountered")) }
        unquote(s).map_err(|e| self.err(pos, &format!("unquoting failed: {e}")))
    }

    /// containerd's parse error: the input with the byte at `pos` marked.
    fn err(&self, pos: usize, msg: &str) -> String {
        let b = self.input.as_bytes();
        if pos < b.len() {
            let s = |x: &[u8]| String::from_utf8_lossy(x).into_owned();
            format!("parse error: [{} >|{}|< {}]: {msg}", s(&b[..pos]), s(&b[pos..pos + 1]), s(&b[pos + 1..]))
        } else {
            format!("parse error: [{}]: {msg}", self.input)
        }
    }
}

const QUOTE_SYNTAX: &str = "quote syntax error";

/// Go's `strconv.Unquote` (containerd's copy of it) for what the scanner gives as quoted: `"…"`, `/…/` or `|…|`,
/// closed, on one line, each escape one Go's scanner takes (so only a value too large for its escape is refused).
/// `/` and `|` quote as `"` does.
fn unquote(s: &str) -> Result<String, &'static str> {
    let b = s.as_bytes();
    let mut rest = &b[1..b.len() - 1];
    let mut out: Vec<u8> = Vec::with_capacity(rest.len());
    // (A character of more than one byte is copied as its bytes are.)
    while let Some((&c, tail)) = rest.split_first() {
        rest = tail;
        if c != b'\\' { out.push(c); continue }
        let (e, tail) = (rest[0], &rest[1..]);
        rest = tail;
        let digits = |n: usize, rest: &mut &[u8], radix: u32| { let (d, t) = rest.split_at(n.min(rest.len())); *rest = t; u32::from_str_radix(std::str::from_utf8(d).unwrap_or_default(), radix).unwrap_or(0) };
        match e {
            b'a' => out.push(7), b'b' => out.push(8), b'f' => out.push(12), b'n' => out.push(10), b'r' => out.push(13), b't' => out.push(9), b'v' => out.push(11),
            b'x' => out.push(digits(2, &mut rest, 16) as u8),
            b'u' | b'U' => {
                let v = digits(if e == b'u' { 4 } else { 8 }, &mut rest, 16);
                if v > 0x10ffff { return Err(QUOTE_SYNTAX) }
                out.extend(char::from_u32(v).unwrap_or('\u{fffd}').to_string().as_bytes());
            }
            b'0'..=b'7' => {
                let v = (e - b'0') as u32 * 64 + digits(2, &mut rest, 8);
                if v > 255 { return Err(QUOTE_SYNTAX) }
                out.push(v as u8);
            }
            // (`\\`, or the quote itself.)
            _ => out.push(e),
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// containerd's own filter tests (`pkg/filters/filter_test.go`), case for case.
    #[test]
    fn containerds_filter_tests() {
        struct E { name: &'static str, other: &'static str, labels: HashMap<&'static str, &'static str> }
        let e = |name, other, labels: &[(&'static str, &'static str)]| E { name, other, labels: labels.iter().copied().collect() };
        let corpus = [
            e("foo", "", &[("foo", "true")]),
            e("bar", "", &[]),
            e("foo", "", &[("foo", "present"), ("more complex label", "present")]),
            e("bar", "", &[("bar", "true")]),
            e("fooer", "", &[("more complex label with \\ and \"", "present")]),
            e("fooer", "", &[("more complex label with \\ and \".post", "present")]),
            e("baz", "too complex, yo", &[]),
            e("bazo", "abc", &[]),
            e("compound", "", &[("foo", "omg_asdf.asdf-qwer")]),
        ];
        let all: Vec<usize> = (0..corpus.len()).collect();
        let cases: Vec<(&str, Result<Vec<usize>, &str>)> = vec![
            ("", Ok(all.clone())),
            ("name", Ok(all.clone())),
            ("labels.foo", Ok(vec![0, 2, 8])),
            ("labels.foo,name", Ok(vec![0, 2, 8])),
            ("labels.foo==true", Ok(vec![0])),
            ("labels.foo==omg_asdf.asdf-qwer", Ok(vec![8])),
            ("labels.|foo|==omg_asdf.asdf-qwer", Err("filters: parse error: [labels. >|||< foo|==omg_asdf.asdf-qwer]: invalid quote encountered")),
            ("name==bar", Ok(vec![1, 3])),
            ("name!=bar", Ok(vec![0, 2, 4, 5, 6, 7, 8])),
            ("name==bar,labels.bar", Ok(vec![3])),
            ("other==\"too complex, yo\"", Ok(vec![6])),
            ("other~=[abc]+,name!=foo", Ok(vec![6, 7])),
            ("other~=/[abc]+/,name!=foo", Ok(vec![6, 7])),
            ("other~=/[abc]{1,2}/,name!=foo", Ok(vec![6, 7])),
            ("other~=/[abc]{0,1}\"\\//,name!=foo", Ok(vec![])),
            ("name==bar,labels.bar==true", Ok(vec![3])),
            ("name==bar,labels.bar==wrong", Ok(vec![])),
            (r#"name==foo,labels."more complex label""#, Ok(vec![2])),
            (r#"labels."more complex label with \\ and \""==present"#, Ok(vec![4])),
            (r#"labels."more complex label with \\ and \"".post==present"#, Ok(vec![5])),
            (r#"labels.?"more complex label with \\ and \"".post==present"#, Err(r#"filters: parse error: [labels. >|?|< "more complex label with \\ and \"".post==present]: expected field or quoted"#)),
            ("name==foo,", Err("filters: parse error: [name==foo,]: expected field or quoted")),
            ("labels.", Err("filters: parse error: [labels.]: expected field or quoted")),
            ("image~=,id?=?fbaq", Err("filters: parse error: [image~= >|,|< id?=?fbaq]: expected value or quoted")),
            ("labels.ns/key==value", Err("filters: parse error: [labels.ns >|/|< key==value]: quoted literal not terminated")),
            ("labels.key==/value", Err("filters: parse error: [labels.key== >|/|< value]: quoted literal not terminated")),
        ];
        for (input, want) in cases {
            match (parse(input), want) {
                (Err(got), Err(want)) => assert_eq!(got, want, "{input}"),
                (Ok(selectors), Ok(want)) => {
                    let f = Filters(if input.is_empty() { vec![] } else { vec![selectors] });
                    let got: Vec<usize> = corpus.iter().enumerate().filter(|(_, c)| {
                        let a = |path: &[String]| -> Option<String> {
                            let (v, present) = match path[0].as_str() {
                                "name" => (c.name.to_string(), !c.name.is_empty()),
                                "other" => (c.other.to_string(), !c.other.is_empty()),
                                "labels" => match c.labels.get(path[1..].join(".").as_str()) { Some(v) => (v.to_string(), true), None => (String::new(), false) },
                                _ => (String::new(), false),
                            };
                            present.then_some(v)
                        };
                        f.matches(&a)
                    }).map(|(i, _)| i).collect();
                    assert_eq!(got, want, "{input}");
                }
                (got, want) => panic!("{input}: {got:?} where {want:?}"),
            }
        }
    }

    /// containerd's scanner tests (`pkg/filters/scanner_test.go`): each token's place, kind and text, and the error
    /// an illegal one leaves.
    #[test]
    fn containerds_scanner_tests() {
        let (f, o, v, q, sep, ill) = (FIELD, OPERATOR, VALUE, QUOTED, SEPARATOR, ILLEGAL);
        let cases: Vec<(&str, Vec<(usize, i32, &str, &str)>)> = vec![
            ("name", vec![(0, f, "name", ""), (4, EOF, "", "")]),
            ("name==value,foo!=bar", vec![(0, f, "name", ""), (4, o, "==", ""), (6, v, "value", ""), (11, sep, ",", ""), (12, f, "foo", ""), (15, o, "!=", ""), (17, v, "bar", ""), (20, EOF, "", "")]),
            ("name==value,labels.foo=value,other.bar~=match", vec![(0, f, "name", ""), (4, o, "==", ""), (6, v, "value", ""), (11, sep, ",", ""), (12, f, "labels", ""), (18, sep, ".", ""), (19, f, "foo", ""), (22, o, "=", ""), (23, v, "value", ""), (28, sep, ",", ""), (29, f, "other", ""), (34, sep, ".", ""), (35, f, "bar", ""), (38, o, "~=", ""), (40, v, "match", ""), (45, EOF, "", "")]),
            ("name~=[abc]+,foo=test", vec![(0, f, "name", ""), (4, o, "~=", ""), (6, v, "[abc]+", ""), (12, sep, ",", ""), (13, f, "foo", ""), (16, o, "=", ""), (17, v, "test", ""), (21, EOF, "", "")]),
            (r"name~=[abc]\+,foo=test", vec![(0, f, "name", ""), (4, o, "~=", ""), (6, v, "[abc]\\+", ""), (13, sep, ",", ""), (14, f, "foo", ""), (17, o, "=", ""), (18, v, "test", ""), (22, EOF, "", "")]),
            ("name~=/[abc]{0,2}/,foo=test", vec![(0, f, "name", ""), (4, o, "~=", ""), (6, q, "/[abc]{0,2}/", ""), (18, sep, ",", ""), (19, f, "foo", ""), (22, o, "=", ""), (23, v, "test", ""), (27, EOF, "", "")]),
            ("name~=牛,labels.moo=true", vec![(0, f, "name", ""), (4, o, "~=", ""), (6, v, "牛", ""), (9, sep, ",", ""), (10, f, "labels", ""), (16, sep, ".", ""), (17, f, "moo", ""), (20, o, "=", ""), (21, v, "true", ""), (25, EOF, "", "")]),
            ("name~=|牛|,labels.moo=true", vec![(0, f, "name", ""), (4, o, "~=", ""), (6, q, "|牛|", ""), (11, sep, ",", ""), (12, f, "labels", ""), (18, sep, ".", ""), (19, f, "moo", ""), (22, o, "=", ""), (23, v, "true", ""), (27, EOF, "", "")]),
            (r#"name~="asdf\n\tfooo""#, vec![(0, f, "name", ""), (4, o, "~=", ""), (6, q, "\"asdf\\n\\tfooo\"", ""), (20, EOF, "", "")]),
            ("foo\0bar", vec![(0, f, "foo", ""), (3, ill, "", "unexpected null"), (4, f, "bar", ""), (7, EOF, "", "")]),
            ("foo = bar    ", vec![(0, f, "foo", ""), (4, o, "=", ""), (6, v, "bar", ""), (13, EOF, "", "")]),
            ("compound.labels==punctuated_value.foo-bar", vec![(0, f, "compound", ""), (8, sep, ".", ""), (9, f, "labels", ""), (15, o, "==", ""), (17, v, "punctuated_value.foo-bar", ""), (41, EOF, "", "")]),
            ("interrupted=", vec![(0, f, "interrupted", ""), (11, o, "=", ""), (12, EOF, "", "")]),
            ("doublevalue=value value", vec![(0, f, "doublevalue", ""), (11, o, "=", ""), (12, v, "value", ""), (18, f, "value", ""), (23, EOF, "", "")]),
            (r#""leading quote".postquote==value"#, vec![(0, q, "\"leading quote\"", ""), (15, sep, ".", ""), (16, f, "postquote", ""), (25, o, "==", ""), (27, v, "value", ""), (32, EOF, "", "")]),
            ("input==,id!=ff", vec![(0, f, "input", ""), (5, o, "==", ""), (7, sep, ",", ""), (8, f, "id", ""), (10, o, "!=", ""), (12, v, "ff", ""), (14, EOF, "", "")]),
            ("input~=/foo\\/bar/,id!=ff", vec![(0, f, "input", ""), (5, o, "~=", ""), (7, q, "/foo\\/bar/", ""), (17, sep, ",", ""), (18, f, "id", ""), (20, o, "!=", ""), (22, v, "ff", ""), (24, EOF, "", "")]),
            ("input~=|foo/bar|,id!=ff", vec![(0, f, "input", ""), (5, o, "~=", ""), (7, q, "|foo/bar|", ""), (16, sep, ",", ""), (17, f, "id", ""), (19, o, "!=", ""), (21, v, "ff", ""), (23, EOF, "", "")]),
            ("labels.containerd.io/key==value", vec![(0, f, "labels", ""), (6, sep, ".", ""), (7, f, "containerd", ""), (17, sep, ".", ""), (18, f, "io", ""), (20, ill, "/key==value", "quoted literal not terminated"), (31, EOF, "", "")]),
            ("labels.\"containerd.io\nkey\"==value", vec![(0, f, "labels", ""), (6, sep, ".", ""), (7, ill, "\"containerd.io\n", "quoted literal not terminated"), (22, f, "key", ""), (25, ill, "\"==value", "quoted literal not terminated"), (33, EOF, "", "")]),
            (r#"labels."\g""#, vec![(0, f, "labels", ""), (6, sep, ".", ""), (7, ill, r#""\g""#, "illegal escape sequence"), (11, EOF, "", "")]),
            (r#"labels."\xaz""#, vec![(0, f, "labels", ""), (6, sep, ".", ""), (7, ill, r#""\xaz""#, "illegal numeric escape sequence"), (13, EOF, "", "")]),
        ];
        for (input, want) in cases {
            let mut sc = Scanner::new(input);
            for (i, w) in want.iter().enumerate() {
                let (pos, tok, text) = sc.scan();
                let err = if tok == ILLEGAL { sc.err.clone() } else { String::new() };
                assert_eq!((pos, tok, text.as_str(), err.as_str()), *w, "{input:?}, token {i}");
            }
            assert_eq!(sc.scan().1, EOF, "{input:?} all read");
        }
    }

    /// Quoting at its edges: the last code point Unicode has and one past it, the highest octal byte and one past it,
    /// hex digits either case, a NUL inside quotes (a character, not the end), and quotes never closed.
    #[test]
    fn quoting_holds_at_its_edges() {
        assert_eq!(unquote("\"\\U0010ffff\""), Ok("\u{10ffff}".to_string()));
        assert_eq!(unquote("\"\\U00110000\""), Err(QUOTE_SYNTAX));
        assert_eq!(unquote("\"\\101\""), Ok("A".to_string()));
        assert!(unquote("\"\\377\"").is_ok() && unquote("\"\\400\"") == Err(QUOTE_SYNTAX));
        assert_eq!([digit_val('a' as i32), digit_val('A' as i32), digit_val('f' as i32), digit_val('F' as i32), digit_val('9' as i32), digit_val('g' as i32)], [10, 10, 15, 15, 9, 16]);
        let a = |path: &[String]| (path[0] == "name").then(|| "a\0b".to_string());
        assert!(Filters::parse_all(&["name==\"a\0b\""]).unwrap().matches(&a));
        assert!(Filters::parse_all(&["name==\"abc"]).unwrap_err().contains("quoted literal not terminated"), "{}", Filters::parse_all(&["name==\"abc"]).unwrap_err());
    }

    #[test]
    fn several_filters_match_when_any_does_and_a_bad_one_is_an_invalid_argument() {
        let a = |path: &[String]| (path[0] == "name").then(|| "x".to_string());
        assert!(Filters::parse_all(&["name==y", "name==x"]).unwrap().matches(&a));
        assert!(!Filters::parse_all(&["name==y"]).unwrap().matches(&a));
        assert!(Filters::parse_all::<&str>(&[]).unwrap().matches(&a));
        assert_eq!(Filters::parse_all(&["name=="]).unwrap_err(), "filters: parse error: [name==]: expected value or quoted: invalid argument");
    }
}
