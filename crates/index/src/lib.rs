//! skwird's index (`docs/DESIGN.md`, decision 8): every name below a
//! root, in folder blocks laid out depth first, so that a folder's whole
//! subtree is one range, cut into chunks that a change rewrites alone.

pub mod block;
pub mod chunk;
pub mod pages;
pub mod tree;
