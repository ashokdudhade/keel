use crate::l::Config;

pub fn get_port(c: &Config) -> u16 {
    c.port
}

pub fn has_port(c: &Config) -> bool {
    c.port > 0
}
