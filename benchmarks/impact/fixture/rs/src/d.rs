use crate::a::alpha;

pub struct Worker;

impl Worker {
    pub fn run(&self) -> i32 {
        alpha()
    }
}
