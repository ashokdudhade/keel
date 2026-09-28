use crate::h::LIMIT;

pub fn check(n: i32) -> bool {
    n < LIMIT
}

pub fn doubled() -> i32 {
    LIMIT * 2
}
