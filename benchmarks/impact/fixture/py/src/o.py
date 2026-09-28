from py.src.m import Gadget


# The receiver is deliberately named after the *other* module: only the
# import (not the qualifier text) identifies the target.
def show(l):
    return l.describe()
