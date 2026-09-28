use crate::e::Config;

// the port method
pub fn get(c: &Config) -> u16 {
    c.port()
}
