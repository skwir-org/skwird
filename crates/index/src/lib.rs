//! skwird's index (`docs/DESIGN.md`, decision 8): every name below a
//! root, in folder blocks laid out depth first, so that a folder's whole
//! subtree is one range, cut into chunks that a change rewrites alone.
//!
//! - [`build`] makes an index from a crawl;
//! - [`Index::query`] answers a [`Query`] below a folder;
//! - [`store`] saves an index to a folder of its own, and maps it back.

pub mod block;
pub mod build;
pub mod chunk;
pub mod index;
pub mod keeper;
pub mod log;
pub mod map;
pub mod matching;
pub mod names;
pub mod packed;
pub mod packer;
pub mod pages;
pub mod store;
pub mod totals;
pub mod tree;
pub mod update;

pub use build::build;
pub use index::{Hit, Index, Query, Sight, SightOf, Sizes};
pub use matching::{NameQuery, Pattern};
pub use update::{Coverage, Hole, Proof, Refreshed, refresh};
