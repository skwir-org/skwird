//! A chunk: a run of folder blocks, in pre-order.
//!
//! The spike's chunk keeps its blocks as they are; the format packs them
//! behind the same calls.

use crate::block::Block;

#[derive(Debug)]
pub struct Chunk {
    blocks: Vec<Block>,
    items: usize,
}

impl Chunk {
    pub fn new(blocks: Vec<Block>) -> Chunk {
        let items = blocks.iter().map(|b| b.items.len() + 1).sum();
        Chunk { blocks, items }
    }

    /// Its blocks, as they are to be rewritten.
    pub fn decode(&self) -> Vec<Block> {
        self.blocks.clone()
    }

    pub fn block(&self, i: usize) -> Block {
        self.blocks[i].clone()
    }

    pub fn depth(&self, i: usize) -> u32 {
        self.blocks[i].depth
    }

    /// Its blocks.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Its weight: its entries, and one for each block.
    pub fn items(&self) -> usize {
        self.items
    }
}
