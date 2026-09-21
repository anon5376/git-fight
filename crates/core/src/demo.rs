//! Built-in conflict so the browser demo can run without a git repo.

use crate::conflict::{ConflictFile, Pick};

pub const DEMO_CONFLICT: &str = concat!(
    "fn version() -> &'static str {\n",
    "<<<<<<< ours\n",
    "    \"0.2.0\"\n",
    "=======\n",
    "    \"0.1.0\"\n",
    ">>>>>>> theirs\n",
    "}\n",
);

pub fn demo_file() -> ConflictFile {
    ConflictFile::parse(DEMO_CONFLICT.as_bytes()).expect("demo conflict is well-formed")
}

pub fn resolve_demo(pick: Pick) -> String {
    String::from_utf8(demo_file().resolve_all(pick)).expect("demo resolve is utf-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conflict::Pick;

    #[test]
    fn demo_parses_one_hunk() {
        let f = demo_file();
        assert_eq!(f.hunk_count(), 1);
        assert_eq!(f.ours(0), b"    \"0.2.0\"\n");
        assert_eq!(f.theirs(0), b"    \"0.1.0\"\n");
        assert!(resolve_demo(Pick::Ours).contains("0.2.0"));
        assert!(!resolve_demo(Pick::Ours).contains("<<<<<<<"));
    }
}
