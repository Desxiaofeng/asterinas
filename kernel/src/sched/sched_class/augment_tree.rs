use alloc::rc::{Rc, Weak};
use core::cell::RefCell;
use alloc::sync::Arc;
use ostd::{
    cpu::{num_cpus, CpuId},
    task::{
        scheduler::{EnqueueFlags, UpdateFlags},
        Task,
    },
};
use core::cmp::min;

#[derive(Debug)]
pub(super) struct AugmentTree {
    pub root: Option<Rc<RefCell<AugmentTreeNode>>>,
}
impl AugmentTree {
    pub fn new() -> Self {
        AugmentTree {
            root: None,
        }
    }

    pub fn up_update(&self, node: Rc<RefCell<AugmentTreeNode>>) -> bool {
        let mut parent = Rc::clone(&node);
        let mut current = Rc::clone(&node);
        loop {
            let mut min_ve = u64::MAX;
            if let Some(left) = current.borrow().left_child.as_ref() {
                min_ve = min(min_ve, left.borrow().min_eligible_vruntime);
            }
            if let Some(right) = current.borrow().right_child.as_ref() {
                min_ve = min(min_ve, right.borrow().min_eligible_vruntime);
            }
            min_ve = min(min_ve, current.borrow().eligible_vruntime);
            {
                current.borrow_mut().min_eligible_vruntime = min_ve;
            }
            match current.borrow().parent.as_ref() {
                Some(parent_node) => {
                    parent = parent_node.upgrade().unwrap();
                }
                None => {
                    return true;
                }
            }
            current = Rc::clone(&parent);
        }
    }

    pub fn insert(&mut self, mut insert_node: AugmentTreeNode) -> bool {
        match &self.root {
            Some(root) => {
                let mut current_node = Rc::clone(root); 
                let mut next_node = Rc::clone(root);

                loop {
                    if insert_node.vruntime_deadline < current_node.borrow().vruntime_deadline {
                        if let Some(child) = &current_node.borrow().left_child {
                            next_node = Rc::clone(child);
                        } else {
                            insert_node.parent = Some(Rc::downgrade(&current_node));
                            current_node.borrow_mut().left_child = Some(Rc::new(RefCell::new(insert_node)));
                            self.up_update(current_node.clone());
                            return true;
                        }
                    } else if insert_node.vruntime_deadline > current_node.borrow().vruntime_deadline {
                        if let Some(child) = &current_node.borrow().right_child {
                            next_node = Rc::clone(child);
                        } else {
                            insert_node.parent = Some(Rc::downgrade(&current_node));
                            current_node.borrow_mut().right_child = Some(Rc::new(RefCell::new(insert_node)));
                            self.up_update(current_node.clone());
                            return true;
                        }
                    } else {
                        return false;
                    };
                    current_node = next_node;
                }
            }
            None => {
                self.root = Some(Rc::new(RefCell::new(insert_node)));
            }
        }
        true
    }

    pub fn delete(&mut self, delete_node: Rc<RefCell<AugmentTreeNode>>) -> bool {
        if delete_node.borrow().left_child.is_none() || delete_node.borrow().right_child.is_none() {
            if delete_node.borrow().left_child.is_none() && delete_node.borrow().right_child.is_none() {
                if delete_node.borrow().parent.is_none() {
                    self.root = None;
                    return true;
                }else {
                    let parent = delete_node.borrow().parent.as_ref().unwrap().upgrade().unwrap().clone();
                    if parent.borrow().left_child.is_some() && Rc::ptr_eq(&delete_node, &parent.borrow().left_child.as_ref().unwrap()) {
                        parent.borrow_mut().left_child = None;
                        self.up_update(parent);
                        return true;
                    } else {
                        parent.borrow_mut().right_child = None;
                        self.up_update(parent);
                        return true;
                    }
                }
            } else {
                if delete_node.borrow().left_child.is_some() {
                    if delete_node.borrow().parent.is_none() {
                        let node = delete_node.borrow().left_child.as_ref().unwrap().clone();
                        node.borrow_mut().parent = None;
                        self.root = Some(node);
                        
                        return true;
                    }else {
                        let parent = delete_node.borrow().parent.as_ref().unwrap().upgrade().unwrap().clone();
                        if parent.borrow().left_child.is_some() && Rc::ptr_eq(&delete_node, &parent.borrow().left_child.as_ref().unwrap()) {
                            parent.borrow_mut().left_child = Some(delete_node.borrow().left_child.as_ref().unwrap().clone());
                            delete_node.borrow().left_child.as_ref().unwrap().borrow_mut().parent = Some(Rc::downgrade(&parent));
                            self.up_update(parent);
                            return true;
                        } else {
                            parent.borrow_mut().right_child = Some(delete_node.borrow().left_child.as_ref().unwrap().clone());
                            delete_node.borrow().left_child.as_ref().unwrap().borrow_mut().parent = Some(Rc::downgrade(&parent));
                            self.up_update(parent);
                            return true;
                        }
                    }
                } else {
                    if delete_node.borrow().parent.is_none() {
                        delete_node.borrow().right_child.as_ref().unwrap().borrow_mut().parent = None;
                        self.root = Some(delete_node.borrow().right_child.as_ref().unwrap().clone());
                        return true;
                    }else {
                        let parent = delete_node.borrow().parent.as_ref().unwrap().upgrade().unwrap().clone();
                        if parent.borrow().left_child.is_some() && Rc::ptr_eq(&delete_node, &parent.borrow().left_child.as_ref().unwrap()) {
                            parent.borrow_mut().left_child = Some(delete_node.borrow().right_child.as_ref().unwrap().clone());
                            delete_node.borrow().right_child.as_ref().unwrap().borrow_mut().parent = Some(Rc::downgrade(&parent));
                            self.up_update(parent);
                            return true;
                        } else {
                            parent.borrow_mut().right_child = Some(delete_node.borrow().right_child.as_ref().unwrap().clone());
                            delete_node.borrow().right_child.as_ref().unwrap().borrow_mut().parent = Some(Rc::downgrade(&parent));
                            self.up_update(parent);
                            return true;
                        }
                    }
                }
            }
        } else {
            let t = delete_node.borrow().right_child.as_ref().unwrap().clone();
            let sussesor = self.find_succesor(t);
            sussesor.borrow_mut().left_child = Some(delete_node.borrow().left_child.as_ref().unwrap().clone());
            delete_node.borrow().left_child.as_ref().unwrap().borrow_mut().parent = Some(Rc::downgrade(&sussesor));
            sussesor.borrow_mut().right_child = Some(delete_node.borrow().right_child.as_ref().unwrap().clone());
            delete_node.borrow().right_child.as_ref().unwrap().borrow_mut().parent = Some(Rc::downgrade(&sussesor));
            if delete_node.borrow().parent.is_none() {
                self.root = Some(sussesor.clone());
            }
            sussesor.borrow_mut().parent = delete_node.borrow().parent.clone();
            self.up_update(sussesor.clone());
        }
        true
    }

    fn find_succesor(&mut self, delete_node: Rc<RefCell<AugmentTreeNode>>) -> Rc<RefCell<AugmentTreeNode>> {
        let mut current = delete_node.clone();
        let mut next = delete_node.clone();
        loop {
            match current.borrow().left_child.as_ref() {
                Some(node) => {
                    next = node.clone();
                }
                None => {
                    break;
                }
            }
            current = next;
        }
        self.delete(current.clone());
        return current.clone();
    }

    pub fn pick(&self, ve: u64) -> Option<Rc<RefCell<AugmentTreeNode>>> {
        let result;
        let mut current = match &self.root {
            Some(node) => {
                Rc::clone(node)
            }
            None => {
                return None;
            }
        };
        let mut next_node = Rc::clone(&current);

        loop {
            let goto_left = if let Some(node) = &current.borrow().left_child {
                if node.borrow().min_eligible_vruntime <= ve {
                    next_node = Rc::clone(node);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            
            if goto_left == false {
                if current.borrow().eligible_vruntime <= ve {
                    result = Some(Rc::clone(&current));
                    break;
                }else {
                    if let Some(node) = &current.borrow().right_child {
                        next_node = Rc::clone(node);
                    } else {
                        return None;
                    }
                }
            }
            current = Rc::clone(&next_node);
        };

        return result;
    }

    fn clear(&mut self) {
        self.root = None;
    }
}

#[derive(Debug)]
pub struct AugmentTreeNode {
    pub eligible_vruntime: u64,
    pub vruntime_deadline: u64,
    pub min_eligible_vruntime: u64,
    pub task: Arc<Task>,

    pub parent: Option<Weak<RefCell<AugmentTreeNode>>>,
    pub left_child: Option<Rc<RefCell<AugmentTreeNode>>>,
    pub right_child: Option<Rc<RefCell<AugmentTreeNode>>>,
}
impl AugmentTreeNode {
    pub fn new(eligible_vruntime: u64, vruntime_deadline: u64, task: Arc<Task>) -> Self {
        AugmentTreeNode {
            eligible_vruntime,
            vruntime_deadline,
            min_eligible_vruntime: eligible_vruntime,
            task: task,
            parent: None,
            left_child: None,
            right_child: None,
        }
    }
}