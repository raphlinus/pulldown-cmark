// Copyright 2018 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

//! A Vec-based container for a tree structure.

use std::num::NonZeroUsize;

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum TreePointer {
    Nil,
    Valid(NonZeroUsize),
}

impl TreePointer {
    pub fn unwrap(self) -> NonZeroUsize {
        match self {
            TreePointer::Nil => panic!("Called unwrap on a Nil value"),
            TreePointer::Valid(ix) => ix,
        }
    }
}

#[derive(Debug)]
pub struct Node<T> {
    pub child: TreePointer,
    pub next: TreePointer,
    pub item: T,
}

/// A tree abstraction, intended for fast building as a preorder traversal.
pub struct Tree<T> {
    nodes: Vec<Node<T>>,
    spine: Vec<NonZeroUsize>, // indices of nodes on path to current node
    cur: TreePointer,
}

impl<T: Default> Tree<T> {
    // Indices start at one, so we place a dummy value at index zero.
    // The alternative would be subtracting one from every NonZeroUsize
    // every time we convert it to usize to index our nodes.
    pub fn new() -> Tree<T> {
        Tree {
            nodes: vec![Node {
                child: TreePointer::Nil,
                next: TreePointer::Nil,
                item: <T as Default>::default(),
            }],
            spine: Vec::new(),
            cur: TreePointer::Nil,
        }
    }

    /// Returns the index of the element currently in focus.
    pub fn cur(&self) -> TreePointer {
        self.cur
    }

    /// Append one item to the current position in the tree.
    pub fn append(&mut self, item: T) -> NonZeroUsize {
        let ix = self.create_node(item);
        let this = TreePointer::Valid(ix);

        if let TreePointer::Valid(ix) = self.cur {
            self[ix].next = this;
        } else if let Some(&parent) = self.spine.last() {
            self[parent].child = this;
        }
        self.cur = this;
        ix
    }

    /// Create an isolated node.
    pub fn create_node(&mut self, item: T) -> NonZeroUsize {
        let this = self.nodes.len();
        self.nodes.push(Node {
            child: TreePointer::Nil,
            next: TreePointer::Nil,
            item,
        });
        NonZeroUsize::new(this).unwrap()
    }

    /// Push down one level, so that new items become children of the current node.
    /// The new focus index is returned.
    pub fn push(&mut self) {
        let cur_ix = self.cur.unwrap();
        self.spine.push(cur_ix);
        self.cur = self[cur_ix].child;
    }

    /// Pop back up a level.
    pub fn pop(&mut self) -> Option<NonZeroUsize> {
        let ix = self.spine.pop()?;
        self.cur = TreePointer::Valid(ix);
        Some(ix)
    }

    /// Look at the parent node.
    pub fn peek_up(&self) -> Option<NonZeroUsize> {
        self.spine.last().cloned()
    }

    /// Look at grandparent node.
    pub fn peek_grandparent(&self) -> Option<NonZeroUsize> {
        if self.spine.len() >= 2 {
            Some(self.spine[self.spine.len() - 2])
        } else {
            None
        }
    }

    /// Returns true when there are no nodes in the tree, false otherwise.
    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    /// Returns the length of the spine.
    pub fn spine_len(&self) -> usize {
        self.spine.len()
    }

    /// Resets the focus to the first node added to the tree, if it exists.
    pub fn reset(&mut self) {
        self.cur = if self.is_empty() {
            TreePointer::Nil
        } else {
            TreePointer::Valid(NonZeroUsize::new(1).unwrap())
        };
        self.spine.truncate(0);
    }

    /// Walks the spine from a root node up to, but not including, the current node.
    pub fn walk_spine(&self) -> impl Iterator<Item = &NonZeroUsize> {
        self.spine.iter()
    }

    /// Moves focus to the next sibling of the current focus.
    pub fn next_sibling(&mut self) {
        self.cur = self[self.cur.unwrap()].next;
    }
}

impl<T> std::ops::Index<NonZeroUsize> for Tree<T> {
    type Output = Node<T>;

    fn index(&self, ix: NonZeroUsize) -> &Self::Output {
        self.nodes.index(ix.get())
    }
}

impl<T> std::ops::IndexMut<NonZeroUsize> for Tree<T> {
    fn index_mut(&mut self, ix: NonZeroUsize) -> &mut Node<T> {
        self.nodes.index_mut(ix.get())
    }
}
