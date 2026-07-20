/// Mentions AuthService in a comment should not confuse a structural index.
pub struct AuthService;

pub fn create_order() {}

pub fn run() {
    // call site — not a definition
    create_order();
}
