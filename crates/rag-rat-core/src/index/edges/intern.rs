//! String interning for the full-rebuild edge accumulator.
//!
//! The full rebuild holds every symbol and edge candidate in memory until the single
//! resolve-and-insert pass (see the parse-once pipeline memory). With one owned `String` per field
//! that accumulator is dominated by heap strings — for the Linux kernel, ~11M edge candidates ×
//! five `Option<String>` is most of peak RSS, even though the *distinct* strings (symbol names,
//! qualified names) number only in the low millions. Interning collapses that: each candidate
//! holds a 4-byte [`Sym`] id, every distinct string lives once in the arena, and the strings are
//! materialised back to `&str` only at resolution/insert time (when the arena is frozen).
//!
//! Why ids, not `&str` slices into the arena: the arena grows wave by wave during accumulation, so
//! any borrow into it would be invalidated by the next intern. `Sym` indices stay valid across
//! growth; we dereference them only after the last wave, when no more interning happens.

use std::collections::HashMap;

/// Interned-string id — an index into [`StrArena::strings`]. 4 bytes, `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Sym(u32);

/// Optional interned-string id packed into 4 bytes: `u32::MAX` is the niche for `None`, so an
/// optional field costs a `u32`, not `Option<Sym>` (8 bytes) or `Option<String>` (24 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct OptSym(u32);

impl OptSym {
    pub(crate) const NONE: OptSym = OptSym(u32::MAX);

    fn some(sym: Sym) -> OptSym {
        OptSym(sym.0)
    }

    fn get(self) -> Option<Sym> {
        if self.0 == u32::MAX { None } else { Some(Sym(self.0)) }
    }
}

/// Append-only string interner. Distinct strings are stored once; lookups dedupe on insert.
///
/// The `index` map double-stores its keys (a `Box<str>` alongside the one in `strings`). That waste
/// is bounded by the *distinct* string count (low millions even for the kernel — tens of MB), not
/// the candidate count, so it stays negligible against the heap it removes from the accumulator.
#[derive(Default)]
pub(crate) struct StrArena {
    strings: Vec<Box<str>>,
    index: HashMap<Box<str>, u32>,
}

impl StrArena {
    pub(crate) fn intern(&mut self, value: &str) -> Sym {
        if let Some(&id) = self.index.get(value) {
            return Sym(id);
        }
        let id = u32::try_from(self.strings.len()).expect("interned string count exceeds u32");
        let boxed: Box<str> = value.into();
        self.strings.push(boxed.clone());
        self.index.insert(boxed, id);
        Sym(id)
    }

    pub(crate) fn intern_opt(&mut self, value: Option<&str>) -> OptSym {
        match value {
            Some(value) => OptSym::some(self.intern(value)),
            None => OptSym::NONE,
        }
    }

    pub(crate) fn get(&self, sym: Sym) -> &str {
        &self.strings[sym.0 as usize]
    }

    pub(crate) fn get_opt(&self, sym: OptSym) -> Option<&str> {
        sym.get().map(|sym| self.get(sym))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_and_dedupes() {
        let mut arena = StrArena::default();
        let a = arena.intern("foo");
        let b = arena.intern("bar");
        let a2 = arena.intern("foo");
        assert_eq!(a, a2, "same string returns the same id");
        assert_ne!(a, b);
        assert_eq!(arena.get(a), "foo");
        assert_eq!(arena.get(b), "bar");
    }

    #[test]
    fn opt_sym_round_trips_and_packs_none() {
        let mut arena = StrArena::default();
        let some = arena.intern_opt(Some("x"));
        let none = arena.intern_opt(None);
        assert_eq!(none, OptSym::NONE);
        assert_eq!(arena.get_opt(some), Some("x"));
        assert_eq!(arena.get_opt(none), None);
        assert_eq!(std::mem::size_of::<OptSym>(), 4, "OptSym must stay 4 bytes");
    }
}
