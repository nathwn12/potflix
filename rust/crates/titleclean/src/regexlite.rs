
/// A bracket expression. `negated` is Java's `[^...]`; ranges are inclusive.
#[derive(Clone, Debug)]
struct CharClass {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl CharClass {
    fn matches(&self, c: char) -> bool {
        let inside = self.ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&c));
        inside != self.negated
    }
}

/// Java's default `.`: any code point except a line terminator.
fn any_char_class() -> CharClass {
    CharClass {
        negated: true,
        ranges: vec![
            ('\n', '\n'),
            ('\r', '\r'),
            ('\u{0085}', '\u{0085}'),
            ('\u{2028}', '\u{2029}'),
        ],
    }
}

fn digit_class() -> CharClass {
    CharClass {
        negated: false,
        ranges: vec![('0', '9')],
    }
}

#[derive(Clone, Debug)]
enum Ast {
    Empty,
    Consume(char),
    Class(CharClass),
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Repeat {
        node: Box<Ast>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
    },
    Group {
        idx: Option<usize>,
        node: Box<Ast>,
    },
    Start,
    End,
}

#[derive(Clone, Copy, Debug)]
enum Inst {
    Consume(char),
    Class(usize),
    /// Prefer the first target; fall back to the second. Greediness is encoded
    /// here, not in a flag, so backtracking order *is* the preference order.
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Start,
    End,
    Match,
}

struct Parser {
    chars: Vec<char>,
    i: usize,
    ncap: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn parse_alt(&mut self) -> Ast {
        let mut alts = vec![self.parse_concat()];
        while self.eat('|') {
            alts.push(self.parse_concat());
        }
        if alts.len() == 1 {
            alts.pop().unwrap_or(Ast::Empty)
        } else {
            Ast::Alt(alts)
        }
    }

    fn parse_concat(&mut self) -> Ast {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.parse_repeat());
        }
        match items.len() {
            0 => Ast::Empty,
            1 => items.pop().unwrap_or(Ast::Empty),
            _ => Ast::Concat(items),
        }
    }

    fn parse_repeat(&mut self) -> Ast {
        let node = self.parse_atom();
        let (min, max) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, None)
            }
            Some('+') => {
                self.i += 1;
                (1, None)
            }
            Some('?') => {
                self.i += 1;
                (0, Some(1))
            }
            Some('{') => match self.bounds() {
                Some((min, max, after)) => {
                    self.i = after;
                    (min, max)
                }
                None => return node,
            },
            _ => return node,
        };
        let greedy = !self.eat('?');
        Ast::Repeat {
            node: Box::new(node),
            min,
            max,
            greedy,
        }
    }

    /// Reads `{n}`, `{n,}` or `{n,m}` starting at the current `{`.
    fn bounds(&self) -> Option<(u32, Option<u32>, usize)> {
        let mut j = self.i + 1;
        let mut lo = String::new();
        while let Some(c) = self.chars.get(j).copied() {
            if c.is_ascii_digit() {
                lo.push(c);
                j += 1;
            } else {
                break;
            }
        }
        if lo.is_empty() {
            return None;
        }
        let min: u32 = lo.parse().ok()?;
        if self.chars.get(j) == Some(&'}') {
            return Some((min, Some(min), j + 1));
        }
        if self.chars.get(j) != Some(&',') {
            return None;
        }
        j += 1;
        let mut hi = String::new();
        while let Some(c) = self.chars.get(j).copied() {
            if c.is_ascii_digit() {
                hi.push(c);
                j += 1;
            } else {
                break;
            }
        }
        if self.chars.get(j) != Some(&'}') {
            return None;
        }
        let max = if hi.is_empty() {
            None
        } else {
            Some(hi.parse().ok()?)
        };
        Some((min, max, j + 1))
    }

    fn parse_atom(&mut self) -> Ast {
        let Some(c) = self.bump() else {
            return Ast::Empty;
        };
        match c {
            '(' => {
                let idx = if self.eat('?') {
                    if !self.eat(':') {
                        panic!("regexlite: only (?:...) groups are supported");
                    }
                    None
                } else {
                    self.ncap += 1;
                    Some(self.ncap)
                };
                let node = self.parse_alt();
                if !self.eat(')') {
                    panic!("regexlite: unbalanced group");
                }
                Ast::Group {
                    idx,
                    node: Box::new(node),
                }
            }
            '[' => Ast::Class(self.parse_class()),
            '\\' => self.parse_escape(),
            '.' => Ast::Class(any_char_class()),
            '^' => Ast::Start,
            '$' => Ast::End,
            other => Ast::Consume(other),
        }
    }

    fn parse_escape(&mut self) -> Ast {
        let Some(c) = self.bump() else {
            return Ast::Consume('\\');
        };
        match c {
            'A' => Ast::Start,
            'z' => Ast::End,
            'd' => Ast::Class(digit_class()),
            'n' => Ast::Consume('\n'),
            'r' => Ast::Consume('\r'),
            't' => Ast::Consume('\t'),
            'f' => Ast::Consume('\u{c}'),
            'v' => Ast::Consume('\u{b}'),
            'x' => Ast::Consume(self.hex()),
            other => Ast::Consume(other),
        }
    }

    fn hex(&mut self) -> char {
        let digits = if self.eat('{') {
            let mut h = String::new();
            while let Some(c) = self.peek() {
                if c == '}' {
                    break;
                }
                h.push(c);
                self.i += 1;
            }
            self.eat('}');
            h
        } else {
            let mut h = String::new();
            for _ in 0..2 {
                if let Some(c) = self.bump() {
                    h.push(c);
                }
            }
            h
        };
        u32::from_str_radix(&digits, 16)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('\u{fffd}')
    }

    fn parse_class(&mut self) -> CharClass {
        let negated = self.eat('^');
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut first = true;
        while let Some(c) = self.peek() {
            if c == ']' && !first {
                self.i += 1;
                break;
            }
            first = false;
            let lo = self.class_atom();
            match lo {
                ClassAtom::Range(a, b) => ranges.push((a, b)),
                ClassAtom::Single(a) => {
                    let dash = self.peek() == Some('-')
                        && self
                            .chars
                            .get(self.i + 1)
                            .copied()
                            .is_some_and(|n| n != ']');
                    if dash {
                        self.i += 1;
                        match self.class_atom() {
                            ClassAtom::Single(b) => ranges.push((a, b)),
                            ClassAtom::Range(b, e) => {
                                ranges.push((a, b));
                                ranges.push((e, e));
                            }
                        }
                    } else {
                        ranges.push((a, a));
                    }
                }
            }
        }
        CharClass { negated, ranges }
    }

    fn class_atom(&mut self) -> ClassAtom {
        let Some(c) = self.bump() else {
            return ClassAtom::Single('\0');
        };
        if c != '\\' {
            return ClassAtom::Single(c);
        }
        match self.bump() {
            Some('x') => ClassAtom::Single(self.hex()),
            Some('n') => ClassAtom::Single('\n'),
            Some('r') => ClassAtom::Single('\r'),
            Some('t') => ClassAtom::Single('\t'),
            Some('f') => ClassAtom::Single('\u{c}'),
            Some('v') => ClassAtom::Single('\u{b}'),
            Some('d') => ClassAtom::Range('0', '9'),
            Some(other) => ClassAtom::Single(other),
            None => ClassAtom::Single('\\'),
        }
    }
}

enum ClassAtom {
    Single(char),
    Range(char, char),
}

#[derive(Default)]
struct Compiler {
    insts: Vec<Inst>,
    classes: Vec<CharClass>,
}

impl Compiler {
    fn here(&self) -> usize {
        self.insts.len()
    }

    fn emit(&mut self, inst: Inst) -> usize {
        self.insts.push(inst);
        self.insts.len() - 1
    }

    fn compile(&mut self, ast: &Ast) {
        match ast {
            Ast::Empty => {}
            Ast::Consume(c) => {
                self.emit(Inst::Consume(*c));
            }
            Ast::Class(cl) => {
                let i = self.classes.len();
                self.classes.push(cl.clone());
                self.emit(Inst::Class(i));
            }
            Ast::Start => {
                self.emit(Inst::Start);
            }
            Ast::End => {
                self.emit(Inst::End);
            }
            Ast::Group { idx, node } => match idx {
                Some(i) => {
                    self.emit(Inst::Save(2 * i));
                    self.compile(node);
                    self.emit(Inst::Save(2 * i + 1));
                }
                None => self.compile(node),
            },
            Ast::Concat(items) => {
                for item in items {
                    self.compile(item);
                }
            }
            Ast::Alt(alts) => {
                let mut jmps = Vec::new();
                for (k, a) in alts.iter().enumerate() {
                    if k + 1 == alts.len() {
                        self.compile(a);
                    } else {
                        let split = self.emit(Inst::Split(0, 0));
                        let body = self.here();
                        self.compile(a);
                        jmps.push(self.emit(Inst::Jmp(0)));
                        let next = self.here();
                        self.insts[split] = Inst::Split(body, next);
                    }
                }
                let end = self.here();
                for j in jmps {
                    self.insts[j] = Inst::Jmp(end);
                }
            }
            Ast::Repeat {
                node,
                min,
                max,
                greedy,
            } => {
                for _ in 0..*min {
                    self.compile(node);
                }
                match max {
                    None => {
                        let top = self.here();
                        let split = self.emit(Inst::Split(0, 0));
                        let body = self.here();
                        self.compile(node);
                        self.emit(Inst::Jmp(top));
                        let after = self.here();
                        self.insts[split] = if *greedy {
                            Inst::Split(body, after)
                        } else {
                            Inst::Split(after, body)
                        };
                    }
                    Some(m) => {
                        let mut splits = Vec::new();
                        for _ in *min..*m {
                            let split = self.emit(Inst::Split(0, 0));
                            let body = self.here();
                            self.compile(node);
                            splits.push((split, body));
                        }
                        let after = self.here();
                        for (split, body) in splits {
                            self.insts[split] = if *greedy {
                                Inst::Split(body, after)
                            } else {
                                Inst::Split(after, body)
                            };
                        }
                    }
                }
            }
        }
    }
}

/// A compiled pattern. Constructed once per pattern (all callers hold theirs in
/// a `LazyLock`), matched many times.
pub struct Regex {
    insts: Vec<Inst>,
    classes: Vec<CharClass>,
    nslots: usize,
    ncap: usize,
    /// The pattern begins with `\A`, so only offset 0 can match.
    anchored: bool,
}

struct Hit {
    caps: Vec<Option<usize>>,
    byte_of: Vec<usize>,
}

impl Hit {
    /// Byte range of capture group `i` (0 is the whole match). `None` is Java's
    /// non-participating (null) group.
    fn span(&self, i: usize) -> Option<(usize, usize)> {
        let start = self.caps[2 * i]?;
        let end = self.caps[2 * i + 1]?;
        Some((self.byte_of[start], self.byte_of[end]))
    }
}

impl Regex {
    /// Compiles `pattern`, panicking on a malformed pattern: every call site
    /// holds a fixed, in-repo pattern, so a failure is a programming error.
    pub fn new(pattern: &str) -> Regex {
        let mut parser = Parser {
            chars: pattern.chars().collect(),
            i: 0,
            ncap: 0,
        };
        let ast = parser.parse_alt();
        if parser.i != parser.chars.len() {
            panic!("regexlite: trailing input in pattern {pattern:?}");
        }
        let ncap = parser.ncap;
        let mut compiler = Compiler::default();
        compiler.emit(Inst::Save(0));
        compiler.compile(&ast);
        compiler.emit(Inst::Save(1));
        compiler.emit(Inst::Match);
        let anchored = matches!(compiler.insts.get(1), Some(Inst::Start));
        Regex {
            insts: compiler.insts,
            classes: compiler.classes,
            nslots: 2 * (ncap + 1),
            ncap,
            anchored,
        }
    }

    /// Byte range of the leftmost-first match.
    pub fn find(&self, hay: &str) -> Option<(usize, usize)> {
        self.search(hay).as_ref().and_then(|hit| hit.span(0))
    }

    pub fn is_match(&self, hay: &str) -> bool {
        self.search(hay).is_some()
    }

    /// Capture groups of the leftmost-first match; `None` entries are
    /// non-participating groups, as in `Matcher.group(i) == null`.
    pub fn captures<'a>(&self, hay: &'a str) -> Option<Vec<Option<&'a str>>> {
        let hit = self.search(hay)?;
        Some(
            (0..=self.ncap)
                .map(|i| hit.span(i).map(|(a, b)| &hay[a..b]))
                .collect(),
        )
    }

    /// Go's `Regexp.ReplaceAllString` for a literal replacement: every
    /// non-overlapping leftmost match is replaced.
    pub fn replace_all(&self, hay: &str, replacement: &str) -> String {
        let mut out = String::with_capacity(hay.len());
        let mut rest = hay;
        loop {
            let Some((start, end)) = self.find(rest) else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..start]);
            out.push_str(replacement);
            if end == start {
                let Some(c) = rest[start..].chars().next() else {
                    break;
                };
                out.push(c);
                rest = &rest[start + c.len_utf8()..];
            } else {
                rest = &rest[end..];
            }
        }
        out
    }

    fn search(&self, hay: &str) -> Option<Hit> {
        let chars: Vec<char> = hay.chars().collect();
        let mut byte_of = Vec::with_capacity(chars.len() + 1);
        for (b, _) in hay.char_indices() {
            byte_of.push(b);
        }
        byte_of.push(hay.len());
        let mut caps = vec![None; self.nslots];
        let starts = if self.anchored {
            0..=0
        } else {
            0..=chars.len()
        };
        for start in starts {
            if self.exec(&chars, 0, start, &mut caps) {
                return Some(Hit { caps, byte_of });
            }
        }
        None
    }

    fn exec(&self, hay: &[char], pc: usize, pos: usize, caps: &mut [Option<usize>]) -> bool {
        match self.insts[pc] {
            Inst::Consume(c) => {
                pos < hay.len() && hay[pos] == c && self.exec(hay, pc + 1, pos + 1, caps)
            }
            Inst::Class(i) => {
                pos < hay.len()
                    && self.classes[i].matches(hay[pos])
                    && self.exec(hay, pc + 1, pos + 1, caps)
            }
            Inst::Split(a, b) => self.exec(hay, a, pos, caps) || self.exec(hay, b, pos, caps),
            Inst::Jmp(t) => self.exec(hay, t, pos, caps),
            Inst::Save(slot) => {
                let old = caps[slot];
                caps[slot] = Some(pos);
                if self.exec(hay, pc + 1, pos, caps) {
                    true
                } else {
                    caps[slot] = old;
                    false
                }
            }
            Inst::Start => pos == 0 && self.exec(hay, pc + 1, pos, caps),
            Inst::End => pos == hay.len() && self.exec(hay, pc + 1, pos, caps),
            Inst::Match => true,
        }
    }
}
