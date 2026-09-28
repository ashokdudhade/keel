package b

import "example.com/impact/a"

func Beta() int {
    return a.Alpha()
}

func Lonely() int {
    return 0
}
