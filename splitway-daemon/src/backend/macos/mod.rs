//! macOS DNS backend: per-domain split-DNS via `/etc/resolver/<domain>` files.
//!
//! Split from the I/O so the file-format logic ([`resolver`]) is a set of pure,
//! unit-tested functions, mirroring the Linux `parser.rs` / backend split. The
//! actual filesystem work lives in [`backend`].

mod backend;
mod demote;
mod resolver;

pub struct MacosBackend;
