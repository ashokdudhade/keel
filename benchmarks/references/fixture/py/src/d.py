from py.src.c import count

def bump():
    global count
    count = count + 1

def reset():
    global count
    count = 0

def get():
    return count
