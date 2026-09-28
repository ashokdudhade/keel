class AppError(Exception):
    pass


def run():
    try:
        work()
    except AppError:
        fix()
