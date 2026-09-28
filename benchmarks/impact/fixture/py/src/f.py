def guard(fn):
    return fn


@guard
def checkout():
    return 1
