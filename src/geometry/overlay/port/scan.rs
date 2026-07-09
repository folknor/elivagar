//! Sweep-line scan structures, collapsed and monomorphized from i_tree 0.19.0.
//!
//! Upstream is a multi-file generic crate (key trees over any `ExpiredKey` /
//! `Expiration`, plus a segment tree). The overlay engine instantiates the key
//! structures only at key = `VSegment`, expiration = i32 (the segment's right
//! x), so this consolidates the used surface - the ordered `KeyExpList` and the
//! red-black `KeyExpTree`, both keyed by VSegment with i32 expiration and
//! generic over the stored value - into one module, fixing those two axes and
//! dropping the unused segment tree (the tree-split keeps its own inlined
//! layout in `solver_tree`). The list search and the red-black balancing are
//! byte-for-byte the upstream algorithm; the pristine reference stays under
//! `research/i_tree`.

use crate::geometry::overlay::port::geom::v_segment::VSegment;
use alloc::vec::Vec;
use core::cmp::Ordering;

const EMPTY_REF: u32 = u32::MAX;

/// A scan structure keyed by `VSegment` with i32 expiration (the segment's
/// right x), storing a `Copy` value per key. Both the ordered-list and the
/// red-black-tree implementations satisfy it.
pub(crate) trait KeyExpCollection<V> {
    fn insert(&mut self, key: VSegment, val: V, time: i32);
    fn first_less(&mut self, time: i32, default: V, key: VSegment) -> V;
    fn first_less_or_equal_by<F>(&mut self, time: i32, default: V, f: F) -> V
    where
        F: Fn(VSegment) -> Ordering;
    fn clear(&mut self);
}

#[derive(Clone, Copy)]
struct Entity<V> {
    key: VSegment,
    val: V,
}

// --- ordered list (i_tree key/list.rs) --------------------------------------

pub(crate) struct KeyExpList<V> {
    buffer: Vec<Entity<V>>,
    min_exp: i32,
}

impl<V: Copy> KeyExpList<V> {
    #[inline(always)]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity),
            min_exp: i32::MAX,
        }
    }

    #[inline(always)]
    pub(crate) fn reserve_capacity(&mut self, capacity: usize) {
        let additional = capacity.saturating_sub(self.buffer.capacity());
        if additional > 0 {
            self.buffer.reserve(additional);
        }
    }

    #[inline]
    fn clear_expired(&mut self, time: i32) {
        if self.min_exp > time {
            return;
        }
        let mut new_min_exp = i32::MAX;
        self.buffer.retain(|s| {
            let exp = s.key.b.x;
            let keep = exp > time;
            if keep {
                new_min_exp = new_min_exp.min(exp);
            }
            keep
        });
        self.min_exp = new_min_exp;
    }
}

impl<V: Copy> KeyExpCollection<V> for KeyExpList<V> {
    #[inline]
    fn insert(&mut self, key: VSegment, val: V, time: i32) {
        self.clear_expired(time);
        self.min_exp = self.min_exp.min(key.b.x);
        let index = self
            .buffer
            .binary_search_by_key(&key, |e| e.key)
            .unwrap_or_else(|index| index);
        self.buffer.insert(index, Entity { key, val });
    }

    #[inline]
    fn first_less(&mut self, time: i32, default: V, key: VSegment) -> V {
        self.clear_expired(time);
        let index = self
            .buffer
            .binary_search_by(|e| e.key.cmp(&key))
            .unwrap_or_else(|index| index);

        if index > 0 {
            unsafe { self.buffer.get_unchecked(index - 1) }.val
        } else {
            default
        }
    }

    #[inline]
    fn first_less_or_equal_by<F>(&mut self, time: i32, default: V, f: F) -> V
    where
        F: Fn(VSegment) -> Ordering,
    {
        self.clear_expired(time);
        match self.buffer.binary_search_by(|e| f(e.key)) {
            Ok(index) => unsafe { self.buffer.get_unchecked(index) }.val,
            Err(index) => {
                if index > 0 {
                    unsafe { self.buffer.get_unchecked(index - 1) }.val
                } else {
                    default
                }
            }
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.min_exp = i32::MAX;
        self.buffer.clear();
    }
}

// --- red-black tree (i_tree key/{tree,node,pool}.rs) ------------------------

#[derive(PartialEq, Clone, Copy)]
enum Color {
    Red,
    Black,
}

#[derive(Clone, Copy)]
struct Node<V> {
    parent: u32,
    left: u32,
    right: u32,
    color: Color,
    entity: Entity<V>,
}

impl<V: Copy> Node<V> {
    #[inline(always)]
    fn is_not_expired(&self, time: i32) -> bool {
        self.entity.key.b.x > time
    }
}

impl<V: Copy> Default for Node<V> {
    #[inline]
    fn default() -> Self {
        Self {
            parent: 0,
            left: 0,
            right: 0,
            color: Color::Red,
            entity: unsafe { core::mem::zeroed() },
        }
    }
}

struct Pool<V> {
    buffer: Vec<Node<V>>,
    unused: Vec<u32>,
}

impl<V: Copy> Pool<V> {
    #[inline(always)]
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(8);
        let mut store = Self {
            buffer: Vec::with_capacity(capacity),
            unused: Vec::with_capacity(capacity),
        };
        store.reserve(capacity);
        store
    }

    #[inline]
    fn reserve(&mut self, additional: usize) {
        debug_assert!(additional > 0);
        let n = self.buffer.len() as u32;
        let l = additional as u32;
        self.buffer.reserve(additional);
        self.buffer
            .resize(self.buffer.len() + additional, Node::default());
        self.unused.reserve(additional);
        self.unused.extend((n..n + l).rev());
    }

    #[inline(always)]
    fn get_free_index(&mut self) -> u32 {
        if self.unused.is_empty() {
            self.reserve(self.unused.capacity());
        }
        self.unused
            .pop()
            .expect("pool reserve guarantees a free index")
    }

    #[inline(always)]
    fn put_back(&mut self, index: u32) {
        self.unused.push(index)
    }
}

pub(crate) struct KeyExpTree<V> {
    store: Pool<V>,
    root: u32,
}

const NIL_INDEX: u32 = 0;

impl<V: Copy> KeyExpTree<V> {
    #[inline]
    pub(crate) fn new(capacity: usize) -> Self {
        let mut store = Pool::new(capacity);
        let nil_index = store.get_free_index();
        assert_eq!(nil_index, NIL_INDEX);
        Self {
            store,
            root: EMPTY_REF,
        }
    }

    #[inline]
    pub(crate) fn reserve_capacity(&mut self, capacity: usize) {
        let additional = capacity.saturating_sub(self.store.buffer.capacity());
        if additional > 0 {
            self.store.reserve(additional)
        }
    }
}

impl<V: Copy> KeyExpCollection<V> for KeyExpTree<V> {
    #[inline(always)]
    fn insert(&mut self, key: VSegment, val: V, time: i32) {
        debug_assert!(key.b.x >= time, "The value is already expired");
        self.insert_entity(Entity { key, val }, time);
    }

    #[inline]
    fn first_less(&mut self, time: i32, default: V, key: VSegment) -> V {
        self.search_first_less(time, default, key)
    }

    #[inline]
    fn first_less_or_equal_by<F>(&mut self, time: i32, default: V, f: F) -> V
    where
        F: Fn(VSegment) -> Ordering,
    {
        self.search_first_less_or_equal_by(time, default, f)
    }

    fn clear(&mut self) {
        if self.root == EMPTY_REF {
            return;
        }
        self.store.put_back(self.root);
        self.root = EMPTY_REF;

        let mut n = 1;
        while n > 0 {
            let i0 = self.store.unused.len() - n;
            n = 0;
            for i in i0..self.store.unused.len() {
                let index = self.store.unused[i];
                let node = self.node(index);
                let left = node.left;
                let right = node.right;
                if left != EMPTY_REF {
                    self.store.put_back(left);
                    n += 1;
                }
                if right != EMPTY_REF {
                    self.store.put_back(right);
                    n += 1;
                }
            }
        }
    }
}

impl<V: Copy> KeyExpTree<V> {
    #[inline(always)]
    fn is_black(&self, index: u32) -> bool {
        index == EMPTY_REF || self.node(index).color == Color::Black
    }

    #[inline(always)]
    fn node(&self, index: u32) -> &Node<V> {
        unsafe { self.store.buffer.get_unchecked(index as usize) }
    }

    #[inline(always)]
    fn node_mut(&mut self, index: u32) -> &mut Node<V> {
        unsafe { self.store.buffer.get_unchecked_mut(index as usize) }
    }

    #[inline]
    fn expire_root(&mut self, time: i32) -> u32 {
        let mut index = self.root;

        while index != EMPTY_REF {
            let node = self.node(index);
            if node.is_not_expired(time) {
                return index;
            }
            self.delete_index(index);
            index = self.root;
        }
        index
    }

    #[inline]
    fn expire_left(&mut self, n_index: u32, time: i32) -> u32 {
        let mut index = self.node(n_index).left;

        while index != EMPTY_REF {
            let node = self.node(index);
            if node.is_not_expired(time) {
                return index;
            }
            self.delete_index(index);
            index = self.node(n_index).left;
        }
        index
    }

    #[inline]
    fn expire_right(&mut self, n_index: u32, time: i32) -> u32 {
        let mut index = self.node(n_index).right;

        while index != EMPTY_REF {
            let node = self.node(index);
            if node.is_not_expired(time) {
                return index;
            }
            self.delete_index(index);
            index = self.node(n_index).right;
        }
        index
    }

    #[inline]
    fn create_nil_node(&mut self, parent: u32) {
        let node = self.node_mut(NIL_INDEX);
        node.parent = parent;
        node.left = EMPTY_REF;
        node.right = EMPTY_REF;
        node.color = Color::Red;
    }

    #[inline]
    fn insert_root(&mut self, entity: Entity<V>) {
        let new_index = self.store.get_free_index();
        let new_node = self.node_mut(new_index);
        new_node.parent = EMPTY_REF;
        new_node.left = EMPTY_REF;
        new_node.right = EMPTY_REF;
        new_node.color = Color::Black;
        new_node.entity = entity;
        self.root = new_index;
    }

    #[inline]
    fn search_first_less(&mut self, time: i32, default: V, key: VSegment) -> V {
        let mut index = self.expire_root(time);
        let mut result = default;
        while index != EMPTY_REF {
            let entity = self.node(index).entity;
            match entity.key.cmp(&key) {
                Ordering::Less => {
                    result = entity.val;
                    index = self.expire_right(index, time);
                }
                _ => index = self.expire_left(index, time),
            }
        }

        result
    }

    #[inline]
    fn search_first_less_or_equal_by<F>(&mut self, time: i32, default: V, f: F) -> V
    where
        F: Fn(VSegment) -> Ordering,
    {
        let mut index = self.expire_root(time);
        let mut result = default;
        while index != EMPTY_REF {
            let entity = self.node(index).entity;
            match f(entity.key) {
                Ordering::Equal => return entity.val,
                Ordering::Less => {
                    result = entity.val;
                    index = self.expire_right(index, time);
                }
                Ordering::Greater => index = self.expire_left(index, time),
            }
        }

        result
    }

    #[inline]
    fn insert_entity(&mut self, entity: Entity<V>, time: i32) {
        let mut index = self.expire_root(time);
        if index == EMPTY_REF {
            self.insert_root(entity);
            return;
        }

        let key = entity.key;

        loop {
            let p_index = index;
            if key < self.node(index).entity.key {
                index = self.expire_left(index, time);
                if index == EMPTY_REF {
                    self.insert_as_left(entity, p_index);
                    return;
                }
            } else {
                index = self.expire_right(index, time);
                if index == EMPTY_REF {
                    self.insert_as_right(entity, p_index);
                    return;
                }
            }
        }
    }

    #[inline]
    fn insert_new(&mut self, entity: Entity<V>, p_index: u32) -> u32 {
        let new_index = self.store.get_free_index();
        let new_node = self.node_mut(new_index);
        new_node.parent = p_index;
        new_node.left = EMPTY_REF;
        new_node.right = EMPTY_REF;
        new_node.color = Color::Red;
        new_node.entity = entity;

        new_index
    }

    #[inline]
    fn insert_as_left(&mut self, entity: Entity<V>, p_index: u32) {
        let new_index = self.insert_new(entity, p_index);

        let parent = self.node_mut(p_index);
        parent.left = new_index;

        if parent.color == Color::Red {
            self.fix_red_black_properties_after_insert(new_index, p_index);
        }
    }

    #[inline]
    fn insert_as_right(&mut self, entity: Entity<V>, p_index: u32) {
        let new_index = self.insert_new(entity, p_index);

        let parent = self.node_mut(p_index);
        parent.right = new_index;

        if parent.color == Color::Red {
            self.fix_red_black_properties_after_insert(new_index, p_index);
        }
    }

    fn fix_red_black_properties_after_insert(&mut self, n_index: u32, p_origin: u32) {
        // parent is red!
        let mut p_index = p_origin;
        let g_index = self.node(p_index).parent;
        if g_index == EMPTY_REF {
            self.node_mut(p_index).color = Color::Black;
            return;
        }

        // Case 3: Uncle is red -> recolor parent, grandparent and uncle
        let u_index = self.get_uncle(p_index);

        if u_index != EMPTY_REF && self.node(u_index).color == Color::Red {
            self.node_mut(p_index).color = Color::Black;
            self.node_mut(g_index).color = Color::Red;
            self.node_mut(u_index).color = Color::Black;

            let gg_index = self.node(g_index).parent;
            if gg_index != EMPTY_REF && self.node(gg_index).color == Color::Red {
                self.fix_red_black_properties_after_insert(g_index, gg_index);
            }
        } else if p_index == self.node(g_index).left {
            // Parent is left child of grandparent
            if n_index == self.node(p_index).right {
                self.rotate_left(p_index);
                p_index = n_index;
            }
            self.rotate_right(g_index);
            self.node_mut(p_index).color = Color::Black;
            self.node_mut(g_index).color = Color::Red;
        } else {
            // Parent is right child of grandparent
            if n_index == self.node(p_index).left {
                self.rotate_right(p_index);
                p_index = n_index;
            }
            self.rotate_left(g_index);
            self.node_mut(p_index).color = Color::Black;
            self.node_mut(g_index).color = Color::Red;
        }
    }

    fn rotate_right(&mut self, index: u32) {
        let n = self.node(index);
        let p = n.parent;
        let lt_index = n.left;

        let lt_node = self.node_mut(lt_index);
        let lt_right = lt_node.right;
        lt_node.right = index;

        if lt_right != EMPTY_REF {
            self.node_mut(lt_right).parent = index;
        }

        let node = self.node_mut(index);
        node.left = lt_right;
        node.parent = lt_index;

        self.replace_parents_child(p, index, lt_index);
    }

    fn rotate_left(&mut self, index: u32) {
        let n = self.node(index);
        let p = n.parent;
        let rt_index = n.right;

        let rt_node = self.node_mut(rt_index);
        let rt_left = rt_node.left;
        rt_node.left = index;

        if rt_left != EMPTY_REF {
            self.node_mut(rt_left).parent = index;
        }
        let node = self.node_mut(index);
        node.right = rt_left;
        node.parent = rt_index;

        self.replace_parents_child(p, index, rt_index);
    }

    #[inline]
    fn replace_parents_child(&mut self, parent: u32, old_child: u32, new_child: u32) {
        self.node_mut(new_child).parent = parent;
        if parent == EMPTY_REF {
            self.root = new_child;
            return;
        }

        let p = self.node_mut(parent);
        debug_assert!(
            p.left == old_child || p.right == old_child,
            "Node is not a child of its parent"
        );

        if p.left == old_child {
            p.left = new_child;
        } else {
            p.right = new_child;
        }
    }

    #[inline]
    fn find_left_minimum(&self, mut i: u32) -> u32 {
        while self.node(i).left != EMPTY_REF {
            i = self.node(i).left;
        }
        i
    }

    fn delete_index(&mut self, index: u32) {
        // Node has zero or one child
        let mut delete_index = index;

        let node = self.node(index);
        let mut nd_left = node.left;
        let mut nd_right = node.right;
        let mut nd_parent = node.parent;
        let mut nd_color = node.color;

        // if two children replace node with it left minimum
        if nd_left != EMPTY_REF && nd_right != EMPTY_REF {
            let successor_index = self.find_left_minimum(nd_right);
            let successor = self.node(successor_index);
            let entity = successor.entity;
            nd_parent = successor.parent;
            nd_left = successor.left;
            nd_right = successor.right;
            nd_color = successor.color;

            self.node_mut(index).entity = entity;

            delete_index = successor_index;
        }

        // only one child can be!

        if nd_left != EMPTY_REF {
            self.replace_parents_child(nd_parent, delete_index, nd_left);
            self.fix_red_black_properties_after_delete(nd_left);
        } else if nd_right != EMPTY_REF {
            self.replace_parents_child(nd_parent, delete_index, nd_right);
            self.fix_red_black_properties_after_delete(nd_right);
        } else if nd_parent == EMPTY_REF {
            self.root = EMPTY_REF;
        } else {
            if nd_color == Color::Black {
                self.create_nil_node(nd_parent);
                self.set_nil_parents_child(nd_parent, delete_index);
                self.fix_red_black_properties_after_delete(NIL_INDEX);
                self.fix_parents_nil_child();
            } else {
                self.remove_parents_child(nd_parent, delete_index);
            }
        }

        self.store.put_back(delete_index);
    }

    fn fix_red_black_properties_after_delete(&mut self, n_index: u32) {
        if n_index == self.root {
            return;
        }

        let mut s_index = self.get_sibling(n_index);

        // Case 2: Red sibling
        if self.node(s_index).color == Color::Red {
            self.handle_red_sibling(n_index, s_index);
            s_index = self.get_sibling(n_index)
        }

        let sibling = self.node(s_index);

        // Cases 3+4: Black sibling with two black children
        if self.is_black(sibling.left) && self.is_black(sibling.right) {
            self.node_mut(s_index).color = Color::Red;
            let p_index = self.node(n_index).parent;

            let parent = self.node_mut(p_index);
            if parent.color == Color::Red {
                parent.color = Color::Black;
            } else {
                self.fix_red_black_properties_after_delete(p_index);
            }
        } else {
            self.handle_black_sibling_with_at_least_one_red_child(n_index, s_index);
        }
    }

    fn handle_black_sibling_with_at_least_one_red_child(&mut self, n_index: u32, s_origin: u32) {
        let p_index = self.node(n_index).parent;

        let mut s_index = s_origin;
        let (mut sibling_left, mut sibling_right) = {
            let sibling = self.node(s_origin);
            (sibling.left, sibling.right)
        };

        let node_is_left_child = n_index == self.node(p_index).left;

        // Case 5
        if node_is_left_child && self.is_black(sibling_right) {
            if sibling_left != EMPTY_REF {
                self.node_mut(sibling_left).color = Color::Black;
            }
            self.node_mut(s_index).color = Color::Red;
            self.rotate_right(s_index);
            s_index = self.node(p_index).right;

            let sibling = self.node(s_index);
            sibling_left = sibling.left;
            sibling_right = sibling.right;
        } else if !node_is_left_child && self.is_black(sibling_left) {
            if sibling_right != EMPTY_REF {
                self.node_mut(sibling_right).color = Color::Black;
            }
            self.node_mut(s_index).color = Color::Red;
            self.rotate_left(s_index);
            s_index = self.node(p_index).left;

            let sibling = self.node(s_index);
            sibling_left = sibling.left;
            sibling_right = sibling.right;
        }

        // Case 6
        self.node_mut(s_index).color = self.node(p_index).color;
        self.node_mut(p_index).color = Color::Black;
        if node_is_left_child {
            if sibling_right != EMPTY_REF {
                self.node_mut(sibling_right).color = Color::Black;
            }
            self.rotate_left(p_index)
        } else {
            if sibling_left != EMPTY_REF {
                self.node_mut(sibling_left).color = Color::Black;
            }
            self.rotate_right(p_index)
        }
    }

    fn handle_red_sibling(&mut self, n_index: u32, s_index: u32) {
        self.node_mut(s_index).color = Color::Black;
        let p_index = self.node(n_index).parent;
        let parent = self.node_mut(p_index);

        parent.color = Color::Red;

        if n_index == parent.left {
            self.rotate_left(p_index)
        } else {
            self.rotate_right(p_index)
        }
    }

    #[inline]
    fn get_uncle(&self, p_index: u32) -> u32 {
        let parent = self.node(p_index);
        debug_assert!(parent.parent != EMPTY_REF);
        let grandparent = self.node(parent.parent);

        debug_assert!(
            grandparent.left == p_index || grandparent.right == p_index,
            "Parent is not a child of its grandparent"
        );

        if grandparent.left == p_index {
            grandparent.right
        } else {
            grandparent.left
        }
    }

    #[inline(always)]
    fn get_sibling(&self, n_index: u32) -> u32 {
        let p_index = self.node(n_index).parent;
        let parent = self.node(p_index);
        debug_assert!(n_index == parent.left || n_index == parent.right);
        if n_index == parent.left {
            parent.right
        } else {
            parent.left
        }
    }

    #[inline]
    fn remove_parents_child(&mut self, parent: u32, old_child: u32) {
        let p = self.node_mut(parent);
        debug_assert!(
            p.left == old_child || p.right == old_child,
            "Node is not a child of its parent"
        );

        if p.left == old_child {
            p.left = EMPTY_REF;
        } else {
            p.right = EMPTY_REF;
        }
    }

    #[inline]
    fn set_nil_parents_child(&mut self, parent: u32, old_child: u32) {
        let p = self.node_mut(parent);
        debug_assert!(
            p.left == old_child || p.right == old_child,
            "Node is not a child of its parent"
        );

        if p.left == old_child {
            p.left = NIL_INDEX;
        } else {
            p.right = NIL_INDEX;
        }
    }

    #[inline]
    fn fix_parents_nil_child(&mut self) {
        let p_index = self.node(NIL_INDEX).parent;
        let p = self.node_mut(p_index);
        debug_assert!(
            p.left == NIL_INDEX || p.right == NIL_INDEX,
            "Node is not a child of its parent"
        );

        if p.left == NIL_INDEX {
            p.left = EMPTY_REF;
        } else {
            p.right = EMPTY_REF;
        }
    }
}
