from py.src.j import Config


def get_port(c):
    return c.port


def has_port(c):
    return c.port > 0
