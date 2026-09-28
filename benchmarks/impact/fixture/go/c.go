package c

import "example.com/impact/b"

func Gamma() int {
    return b.Beta() + 1
}
