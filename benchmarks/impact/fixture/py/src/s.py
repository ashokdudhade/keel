import importlib

mod = importlib.import_module("py.src.r")

def boot():
    return mod.serve()
