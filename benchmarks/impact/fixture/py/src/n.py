from py.src.l import Widget


# The receiver is deliberately named after the *other* module: only the
# import (not the qualifier text) identifies the target.
def show(m):
    return m.describe()
