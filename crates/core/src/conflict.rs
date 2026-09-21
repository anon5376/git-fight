//! Git conflict-marker parser and resolver. No I/O.

use std::ops::Range;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pick {
    Ours,
    Theirs,
    Both,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    Binary,
    Broken(&'static str),
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ParseError::Binary => write!(f, "binary file"),
            ParseError::Broken(msg) => write!(f, "broken conflict markers: {msg}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hunk {
    pub full: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
    pub base: Option<Range<usize>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictFile {
    original: Vec<u8>,
    hunks: Vec<Hunk>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Text,
    Ours,
    Base,
    Theirs,
}

impl ConflictFile {
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        if bytes.contains(&0) {
            return Err(ParseError::Binary);
        }

        let mut hunks = Vec::new();
        let mut mode = Mode::Text;
        let mut hunk_start = 0usize;
        let mut ours_start = 0usize;
        let mut base_start = 0usize;
        let mut has_base = false;
        let mut theirs_start = 0usize;
        let mut ours_end = 0usize;
        let mut base_end = 0usize;

        let mut i = 0usize;
        while i < bytes.len() {
            let line_start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            let line = &bytes[line_start..i];

            if starts_with_marker(line, b"<<<<<<<") {
                if mode != Mode::Text {
                    return Err(ParseError::Broken("nested conflict marker"));
                }
                mode = Mode::Ours;
                hunk_start = line_start;
                ours_start = i;
                has_base = false;
            } else if starts_with_marker(line, b"|||||||") {
                if mode != Mode::Ours {
                    return Err(ParseError::Broken("unexpected |||||||"));
                }
                mode = Mode::Base;
                ours_end = line_start;
                base_start = i;
                has_base = true;
            } else if starts_with_marker(line, b"=======") {
                match mode {
                    Mode::Ours => {
                        ours_end = line_start;
                        mode = Mode::Theirs;
                        theirs_start = i;
                    }
                    Mode::Base => {
                        base_end = line_start;
                        mode = Mode::Theirs;
                        theirs_start = i;
                    }
                    _ => return Err(ParseError::Broken("unexpected =======")),
                }
            } else if starts_with_marker(line, b">>>>>>>") {
                if mode != Mode::Theirs {
                    return Err(ParseError::Broken("unexpected >>>>>>>"));
                }
                hunks.push(Hunk {
                    full: hunk_start..i,
                    ours: ours_start..ours_end,
                    theirs: theirs_start..line_start,
                    base: if has_base {
                        Some(base_start..base_end)
                    } else {
                        None
                    },
                });
                mode = Mode::Text;
            }
        }

        if mode != Mode::Text {
            return Err(ParseError::Broken("unclosed conflict marker"));
        }

        Ok(Self {
            original: bytes.to_vec(),
            hunks,
        })
    }

    pub fn original(&self) -> &[u8] {
        &self.original
    }

    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    pub fn ours(&self, hunk: usize) -> &[u8] {
        let r = &self.hunks[hunk].ours;
        &self.original[r.start..r.end]
    }

    pub fn theirs(&self, hunk: usize) -> &[u8] {
        let r = &self.hunks[hunk].theirs;
        &self.original[r.start..r.end]
    }

    pub fn base(&self, hunk: usize) -> Option<&[u8]> {
        let r = self.hunks.get(hunk)?.base.as_ref()?;
        Some(&self.original[r.start..r.end])
    }

    /// Reconstruct the file. `None` leaves that hunk's markers in place.
    /// When every pick is `None`, the original bytes are returned unchanged.
    pub fn resolve(&self, picks: &[Option<Pick>]) -> Vec<u8> {
        if self.hunks.is_empty() {
            return self.original.clone();
        }
        if picks.iter().all(Option::is_none) && picks.len() >= self.hunks.len() {
            return self.original.clone();
        }
        if picks.iter().all(Option::is_none) && picks.is_empty() {
            return self.original.clone();
        }

        let mut out = Vec::with_capacity(self.original.len());
        let mut cursor = 0usize;
        for (idx, hunk) in self.hunks.iter().enumerate() {
            out.extend_from_slice(&self.original[cursor..hunk.full.start]);
            match picks.get(idx).copied().flatten() {
                None => out.extend_from_slice(&self.original[hunk.full.start..hunk.full.end]),
                Some(Pick::Ours) => {
                    out.extend_from_slice(&self.original[hunk.ours.start..hunk.ours.end])
                }
                Some(Pick::Theirs) => {
                    out.extend_from_slice(&self.original[hunk.theirs.start..hunk.theirs.end])
                }
                Some(Pick::Both) => {
                    out.extend_from_slice(&self.original[hunk.ours.start..hunk.ours.end]);
                    out.extend_from_slice(&self.original[hunk.theirs.start..hunk.theirs.end]);
                }
            }
            cursor = hunk.full.end;
        }
        out.extend_from_slice(&self.original[cursor..]);
        out
    }

    pub fn resolve_all(&self, pick: Pick) -> Vec<u8> {
        let picks = vec![Some(pick); self.hunks.len()];
        self.resolve(&picks)
    }
}

fn starts_with_marker(line: &[u8], marker: &[u8]) -> bool {
    let body = strip_newline(line);
    body.starts_with(marker)
}

fn strip_newline(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_without_conflicts() {
        let src = b"hello\nworld\n";
        let f = ConflictFile::parse(src).unwrap();
        assert_eq!(f.hunk_count(), 0);
        assert_eq!(f.resolve(&[]), src);
    }

    #[test]
    fn identity_with_conflicts_unpicked() {
        let src = b"pre\n<<<<<<< ours\nA\n=======\nB\n>>>>>>> theirs\npost\n";
        let f = ConflictFile::parse(src).unwrap();
        assert_eq!(f.hunk_count(), 1);
        assert_eq!(f.resolve(&[]), src);
        assert_eq!(f.resolve(&[None]), src);
        assert_eq!(f.ours(0), b"A\n");
        assert_eq!(f.theirs(0), b"B\n");
    }

    #[test]
    fn pick_ours_theirs_both() {
        let src = b"<<<<<<< ours\nA\n=======\nB\n>>>>>>> theirs\n";
        let f = ConflictFile::parse(src).unwrap();
        assert_eq!(f.resolve_all(Pick::Ours), b"A\n");
        assert_eq!(f.resolve_all(Pick::Theirs), b"B\n");
        assert_eq!(f.resolve_all(Pick::Both), b"A\nB\n");
    }

    #[test]
    fn diff3_markers() {
        let src = b"<<<<<<< ours\nA\n||||||| base\nC\n=======\nB\n>>>>>>> theirs\n";
        let f = ConflictFile::parse(src).unwrap();
        assert!(f.hunks()[0].base.is_some());
        assert_eq!(f.resolve_all(Pick::Ours), b"A\n");
        assert_eq!(f.resolve(&[]), src);
    }

    #[test]
    fn two_hunks_partial_resolve() {
        let src = concat!(
            "<<<<<<< a\n1\n=======\n2\n>>>>>>> b\n",
            "mid\n",
            "<<<<<<< a\n3\n=======\n4\n>>>>>>> b\n"
        )
        .as_bytes();
        let f = ConflictFile::parse(src).unwrap();
        let out = f.resolve(&[Some(Pick::Ours), None]);
        assert_eq!(
            out,
            concat!("1\n", "mid\n", "<<<<<<< a\n3\n=======\n4\n>>>>>>> b\n").as_bytes()
        );
    }

    #[test]
    fn crlf_round_trip() {
        let src = b"<<<<<<< ours\r\nA\r\n=======\r\nB\r\n>>>>>>> theirs\r\n";
        let f = ConflictFile::parse(src).unwrap();
        assert_eq!(f.resolve(&[]), src);
        assert_eq!(f.resolve_all(Pick::Ours), b"A\r\n");
    }

    #[test]
    fn binary_rejected() {
        assert_eq!(ConflictFile::parse(b"a\0b"), Err(ParseError::Binary));
    }

    #[test]
    fn broken_unclosed() {
        let src = b"<<<<<<< ours\nA\n=======\nB\n";
        assert!(matches!(
            ConflictFile::parse(src),
            Err(ParseError::Broken(_))
        ));
    }

    #[test]
    fn broken_unexpected_separator() {
        let src = b"=======\n";
        assert!(matches!(
            ConflictFile::parse(src),
            Err(ParseError::Broken(_))
        ));
    }

    #[test]
    fn marker_in_middle_of_line_is_text() {
        let src = b"code // <<<<<<< not a marker\n";
        let f = ConflictFile::parse(src).unwrap();
        assert_eq!(f.hunk_count(), 0);
        assert_eq!(f.resolve(&[]), src);
    }

    #[test]
    fn property_random_text_identity() {
        let mut rng = crate::Pcg32::new(0x00C0_F1C7);
        for n in 0..200 {
            let len = rng.next_bounded(64) as usize;
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                let b = (rng.next_bounded(96) as u8).saturating_add(32);
                if b != 0 {
                    bytes.push(b);
                }
            }
            if let Ok(f) = ConflictFile::parse(&bytes) {
                assert_eq!(f.resolve(&[]), bytes, "case {n}");
            }
        }
    }

    #[test]
    fn property_generated_conflicts_identity() {
        let mut rng = crate::Pcg32::new(99);
        for _ in 0..80 {
            let mut src = Vec::new();
            let hunks = 1 + rng.next_bounded(3) as usize;
            for h in 0..hunks {
                push_noise(&mut rng, &mut src);
                src.extend_from_slice(b"<<<<<<< ours\n");
                push_noise(&mut rng, &mut src);
                if rng.next_bounded(2) == 0 {
                    src.extend_from_slice(b"||||||| base\n");
                    push_noise(&mut rng, &mut src);
                }
                src.extend_from_slice(b"=======\n");
                push_noise(&mut rng, &mut src);
                src.extend_from_slice(format!(">>>>>>> theirs{h}\n").as_bytes());
            }
            push_noise(&mut rng, &mut src);
            let f = ConflictFile::parse(&src).unwrap();
            assert_eq!(f.hunk_count(), hunks);
            assert_eq!(f.resolve(&[]), src);
        }
    }

    fn push_noise(rng: &mut crate::Pcg32, out: &mut Vec<u8>) {
        let lines = rng.next_bounded(3);
        for _ in 0..lines {
            let n = rng.next_bounded(8);
            for _ in 0..n {
                out.push(b'a' + rng.next_bounded(26) as u8);
            }
            out.push(b'\n');
        }
    }
}
