//! The NovaOS shell language, independent of the machine it runs on: it parses command lines,
//! expands words, matches wildcards and evaluates arithmetic and `test`. The `sh` program adds
//! the parts that need the operating system (processes, files, the terminal). Because none of
//! this touches the OS, all of it is unit-tested on the build machine.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod arith;
pub mod expand;
pub mod glob;
pub mod syntax;
pub mod testexpr;

pub use expand::{expand_single, expand_word, Context};
pub use syntax::{parse, AndOr, Command, Connector, Item, List, ParseError, Part, Pipeline, Redir, RedirOp, VarOp, Word, WordPart};
