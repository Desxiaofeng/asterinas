use alloc::sync::{Arc, Weak};
use aster_logger::print;
use ostd::{
    cpu::{num_cpus, CpuId},
    task::{
        scheduler::{EnqueueFlags, UpdateFlags},
        Task,
    },
};
use spin::Mutex;
#[derive(Debug)]
pub struct AugmentTree {
    pub root: Option<Arc<Mutex<AugmentTreeNode>>>,
    pub len: u64,
}
impl AugmentTree {
    pub fn new() -> Self {
        AugmentTree {
            root: None,
            len: 0,
        }
    }

    fn up_update(&self, node: Arc<Mutex<AugmentTreeNode>>, end: Option<Arc<Mutex<AugmentTreeNode>>>) -> bool {
        let mut current = node;
        loop {
            let mut guard = current.lock();
            let mut min_ve = guard.eligible_vruntime;
            min_ve = min_ve.min(guard
                .left_child.as_ref()
                .map(|tmp| tmp.lock().min_eligible_vruntime)
                .unwrap_or(u64::MAX)
            );
            min_ve = min_ve.min(guard
                .right_child.as_ref()
                .map(|tmp| tmp.lock().min_eligible_vruntime)
                .unwrap_or(u64::MAX)
            );
            guard.min_eligible_vruntime = min_ve;

            match guard.parent.as_ref() {
                Some(parent_node) => {
                    let parent = parent_node.upgrade().unwrap();
                    drop(guard);
                    if end.is_some() && Arc::ptr_eq(&parent, end.as_ref().unwrap()) {
                        return true;
                    }
                    current = parent;
                }
                None => {
                    return true;
                }
            }
        }
    }

    fn _find(&self, vd: u64) -> Option<Arc<Mutex<AugmentTreeNode>>> {
        let mut current = self.root.as_ref().unwrap().clone();
        
        loop {
            let current_guard = current.lock();
            if vd < current_guard.vruntime_deadline {
                if current_guard.left_child.is_some() {
                    let child = current_guard.left_child.as_ref().unwrap().clone();
                    drop(current_guard);
                    current = child;
                } else {
                    drop(current_guard);
                    return Some(current);
                }
            } else if vd >= current_guard.vruntime_deadline {
                if current_guard.right_child.is_some() {
                    let child = current_guard.right_child.as_ref().unwrap().clone();
                    drop(current_guard);
                    current = child;
                } else {
                    drop(current_guard);
                    return Some(current);
                }
            } else {
                return None;
            }
        }
    }

    pub fn insert(&mut self, ev: u64, vd: u64, task: Arc<Task>) -> bool {
        if self.root.is_some() {
            let to_be_inserted = self._find(vd);
            match to_be_inserted {
                Some(node) => {
                    let mut guard = node.lock();
                    if vd < guard.vruntime_deadline {
                        guard.left_child = Some(Arc::new(Mutex::new(AugmentTreeNode::new_with_parent(ev, vd, task, Arc::downgrade(&node)))));

                    } else {
                        guard.right_child = Some(Arc::new(Mutex::new(AugmentTreeNode::new_with_parent(ev, vd, task, Arc::downgrade(&node)))));
                    }
                    drop(guard);
                    self.up_update(node, None);
                }
                // this mean there is a node with same vd
                None => {
                    return false;
                }
            }
        } else {
            self.root = Some(Arc::new(Mutex::new(AugmentTreeNode::new(ev, vd, task))));
        }
        self.len += 1;
        return true;
    }

    pub fn pick(&self, ev: u64) -> Option<Arc<Mutex<AugmentTreeNode>>> {
        if self.len == 0 {
            return None;
        }

        let mut current = self.root.as_ref().unwrap().clone();

        loop {
            let guard = current.lock();
            if let Some(node) = guard.left_child.as_ref() {
                if node.lock().min_eligible_vruntime <= ev{
                    let next = node.clone();
                    drop(guard);
                    current = next;
                    continue;
                }
            }

            // current or right
            if guard.eligible_vruntime <= ev {
                drop(guard);
                return Some(current);
            } else {
                if let Some(node) = guard.right_child.as_ref() {
                    let next = node.clone();
                    drop(guard);
                    current = next;
                    continue;
                } else {
                    return None;
                }
            }
        };
    }

    pub fn delete(&mut self, delete_node: Arc<Mutex<AugmentTreeNode>>, end: Option<Arc<Mutex<AugmentTreeNode>>>) -> bool {
        let mut guard = delete_node.lock();
        let have_left = guard.left_child.is_some();
        let have_right = guard.right_child.is_some();
        let have_parent = guard.parent.is_some();
        let (is_left, parent) = if have_parent {
            let parent = guard.parent.take().unwrap().upgrade(); 
            let is_left = guard.vruntime_deadline < parent.as_ref().unwrap().lock().vruntime_deadline;
            (is_left, parent)
        } else {
            (false, None)
        };
        // drop(guard);

        match (have_left, have_right) {
            (false, false) => {
                if have_parent {
                    if is_left {
                        parent.as_ref().unwrap().lock().left_child = None;
                    } else {
                        parent.as_ref().unwrap().lock().right_child = None;
                    }
                    self.up_update(parent.unwrap(), end);
                } else {
                    self.root = None;
                }
            }
            (true, false) => {
                if have_parent {
                    guard.left_child.as_ref().unwrap().lock().parent = Some(Arc::downgrade(parent.as_ref().unwrap()));
                    if is_left {
                        parent.as_ref().unwrap().lock().left_child = guard.left_child.take();
                    } else {
                        parent.as_ref().unwrap().lock().right_child = guard.left_child.take();
                    }
                    self.up_update(parent.unwrap(), end);
                } else {
                    guard.left_child.as_ref().unwrap().lock().parent = None;
                    self.root = guard.left_child.take();
                }
            }
            (false, true) => {
                if have_parent {
                    guard.right_child.as_ref().unwrap().lock().parent = Some(Arc::downgrade(parent.as_ref().unwrap()));
                    if is_left {
                        parent.as_ref().unwrap().lock().left_child = guard.right_child.take();
                    } else {
                        parent.as_ref().unwrap().lock().right_child = guard.right_child.take();
                    }
                    self.up_update(parent.unwrap(), end);
                } else {
                    guard.right_child.as_ref().unwrap().lock().parent = None;
                    self.root = guard.right_child.take();
                }

            }
            (true, true) => {
                drop(guard);
                let succesor = self.find_succesor(delete_node.clone());
                let mut guard = delete_node.lock();
                let mut succesor_guard = succesor.lock();
                guard.left_child.as_ref().unwrap().lock().parent = Some(Arc::downgrade(&succesor));
                guard.right_child.as_ref().unwrap().lock().parent = Some(Arc::downgrade(&succesor));
                succesor_guard.left_child = guard.left_child.take();
                succesor_guard.right_child = guard.right_child.take();
                

                if have_parent {
                    succesor_guard.parent = Some(Arc::downgrade(parent.as_ref().unwrap()));
                    
                    if is_left {
                        parent.as_ref().unwrap().lock().left_child = Some(succesor.clone());
                    } else {
                        parent.as_ref().unwrap().lock().right_child = Some(succesor.clone());
                    }
                } else {
                    self.root = Some(succesor.clone());
                }
                drop(guard);
                drop(succesor_guard);
                self.up_update(succesor, end);
                self.len += 1;
            }
        }
        self.len -= 1;
        true
    }

    fn find_succesor(&mut self, delete_node: Arc<Mutex<AugmentTreeNode>>) -> Arc<Mutex<AugmentTreeNode>> {
        let mut current = delete_node.lock().right_child.as_ref().unwrap().clone();
        
        loop {
            let guard = current.lock();
            match guard.left_child.as_ref() {
                Some(node) => {
                    let next = node.clone();
                    drop(guard);
                    current = next;
                    continue;
                }
                None => {
                    drop(guard);
                    break;
                }
            }
        }
        self.delete(current.clone(), Some(delete_node));

        return current;
    }
}

#[derive(Debug)]
pub struct AugmentTreeNode {
    pub eligible_vruntime: u64,
    pub vruntime_deadline: u64,
    pub min_eligible_vruntime: u64,
    pub task: Arc<Task>,

    pub parent: Option<Weak<Mutex<AugmentTreeNode>>>,
    pub left_child: Option<Arc<Mutex<AugmentTreeNode>>>,
    pub right_child: Option<Arc<Mutex<AugmentTreeNode>>>,
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
    pub fn new_with_parent(eligible_vruntime: u64, vruntime_deadline: u64, task: Arc<Task>, parent: Weak<Mutex<AugmentTreeNode>>) -> Self {
        AugmentTreeNode {
            eligible_vruntime,
            vruntime_deadline,
            min_eligible_vruntime: eligible_vruntime,
            task: task,

            parent: Some(parent),
            left_child: None,
            right_child: None,
        }
    }
}